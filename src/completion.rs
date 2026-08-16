//! Process-owned mailbox for worker completions posted across threads (SBS-743).
//!
//! Several Matteshot surfaces used to put a `Box` pointer in `LPARAM` and
//! `Box::from_raw` it in the receiving wndproc. A same-integrity process can
//! enumerate those windows and post the private `WM_APP` / `WM_USER` message
//! with an arbitrary non-null `LPARAM`, which is then treated as a pointer:
//! invalid dereference, a crash, or a free of attacker-chosen memory.
//! Private message numbers are not authentication.
//!
//! Completions now live in a process-owned map. Workers insert a payload and
//! post only an opaque integer token. A wndproc may remove a token only when
//! it is still pending for that exact window and generation. Destroyed
//! windows drop their pending entries so a recycled `HWND` cannot take them.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Opaque token carried in `LPARAM`. Never a pointer.
pub type CompletionToken = u64;

pub struct CompletionMailbox<T> {
    next: AtomicU64,
    inner: Mutex<Inner<T>>,
}

struct Inner<T> {
    pending: Vec<Slot<T>>,
    generations: Vec<(isize, u64)>,
}

struct Slot<T> {
    token: CompletionToken,
    hwnd: isize,
    generation: u64,
    payload: T,
}

impl<T> CompletionMailbox<T> {
    pub const fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
            inner: Mutex::new(Inner {
                pending: Vec::new(),
                generations: Vec::new(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<T>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn mint_token(&self) -> CompletionToken {
        // Mix the counter so tokens are not 0/1 (the forged LPARAMs in the
        // SBS-743 acceptance check) and do not look like heap pointers.
        loop {
            let token = splitmix64(self.next.fetch_add(1, Ordering::Relaxed));
            if token > 1 {
                return token;
            }
        }
    }

    /// Store `payload` for `hwnd` at the window's current generation.
    pub fn insert(&self, hwnd: isize, payload: T) -> CompletionToken {
        let token = self.mint_token();
        let mut inner = self.lock();
        let generation = inner.generation(hwnd);
        inner.pending.push(Slot {
            token,
            hwnd,
            generation,
            payload,
        });
        token
    }

    /// Insert, then run `post(token)`. If posting fails, drop the token so
    /// the payload cannot leak for a window that will never receive it.
    pub fn post_with<F>(&self, hwnd: isize, payload: T, post: F)
    where
        F: FnOnce(CompletionToken) -> bool,
    {
        let token = self.insert(hwnd, payload);
        if !post(token) {
            self.discard_token(token);
        }
    }

    /// Take the payload only when `token` is pending for this exact window
    /// and the window's current generation. A forged or stale `LPARAM` is
    /// `None` and does not remove anyone else's entry.
    pub fn take(&self, token: CompletionToken, hwnd: isize) -> Option<T> {
        let mut inner = self.lock();
        let generation = inner.generation(hwnd);
        let index = inner.pending.iter().position(|slot| {
            slot.token == token && slot.hwnd == hwnd && slot.generation == generation
        })?;
        Some(inner.pending.remove(index).payload)
    }

    /// Worker-side cleanup when `PostMessage` fails. The worker still owns
    /// the token it just minted, so this may remove by token alone.
    pub fn discard_token(&self, token: CompletionToken) {
        let mut inner = self.lock();
        inner.pending.retain(|slot| slot.token != token);
    }

    /// Drop every pending entry for `hwnd` and bump its generation so a
    /// recycled handle cannot take a completion posted to the previous one.
    pub fn unbind(&self, hwnd: isize) {
        let mut inner = self.lock();
        inner.pending.retain(|slot| slot.hwnd != hwnd);
        inner.bump_generation(hwnd);
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.lock().pending.len()
    }
}

fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl<T> Inner<T> {
    fn generation(&self, hwnd: isize) -> u64 {
        self.generations
            .iter()
            .find(|(key, _)| *key == hwnd)
            .map(|(_, generation)| *generation)
            .unwrap_or(0)
    }

    fn bump_generation(&mut self, hwnd: isize) {
        if let Some((_, generation)) = self.generations.iter_mut().find(|(key, _)| *key == hwnd) {
            *generation = generation.saturating_add(1);
            return;
        }
        self.generations.push((hwnd, 1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins SBS-743: a forged completion `LPARAM` (0, 1, or an arbitrary
    /// mapped address) must not be treated as a pointer and must not
    /// consume a legitimate pending completion.
    #[test]
    fn forged_lparam_values_do_not_take_or_consume_a_real_completion() {
        let mailbox = CompletionMailbox::new();
        let hwnd = 0x100;
        for forged in [0_u64, 1, 0x7fff_ffff, 0xDEAD_BEEF, u64::MAX] {
            assert!(
                mailbox.take(forged, hwnd).is_none(),
                "empty mailbox accepted forged token {:#x}",
                forged
            );
        }
        let token = mailbox.insert(hwnd, "legit");
        assert!(token > 1, "minted a token the acceptance check forges");

        for forged in [0_u64, 1, 0x7fff_ffff, 0xDEAD_BEEF, u64::MAX] {
            assert_ne!(token, forged);
            assert!(
                mailbox.take(forged, hwnd).is_none(),
                "forged token {:#x} was accepted as a completion",
                forged
            );
        }
        assert_eq!(
            mailbox.take(token, hwnd),
            Some("legit"),
            "a forged LPARAM consumed the real completion"
        );
        assert!(
            mailbox.take(token, hwnd).is_none(),
            "a legitimate completion was delivered more than once"
        );
    }

    /// Pins SBS-743: take requires the exact window that the worker posted to.
    #[test]
    fn a_token_posted_to_one_window_is_not_taken_by_another() {
        let mailbox = CompletionMailbox::new();
        let token = mailbox.insert(0x100, "for-a");
        assert!(mailbox.take(token, 0x200).is_none());
        assert_eq!(mailbox.pending_len(), 1);
        assert_eq!(mailbox.take(token, 0x100), Some("for-a"));
    }

    /// Pins SBS-743: destroying a window drops its pending entries so a
    /// recycled HWND / generation cannot take them.
    #[test]
    fn unbind_drops_pending_entries_and_rejects_the_old_token() {
        let mailbox = CompletionMailbox::new();
        let hwnd = 0x100;
        let token = mailbox.insert(hwnd, "stale");
        mailbox.unbind(hwnd);
        assert_eq!(mailbox.pending_len(), 0);
        assert!(mailbox.take(token, hwnd).is_none());

        let next = mailbox.insert(hwnd, "fresh");
        assert_ne!(next, token);
        assert_eq!(mailbox.take(next, hwnd), Some("fresh"));
    }

    /// Pins SBS-743: minted tokens are never the LPARAM values a helper is
    /// specified to forge (0, 1).
    #[test]
    fn minted_tokens_are_never_the_forged_lparam_values() {
        let mailbox = CompletionMailbox::<u32>::new();
        for i in 0..64 {
            let token = mailbox.insert(0x10, i);
            assert!(token > 1, "minted token {}", token);
        }
    }

    /// Pins SBS-743: a failed post must not leave a token that a later
    /// forged message could still redeem.
    #[test]
    fn a_failed_post_discards_the_token() {
        let mailbox = CompletionMailbox::new();
        mailbox.post_with(0x100, "lost", |_| false);
        assert_eq!(mailbox.pending_len(), 0);
        mailbox.post_with(0x100, "kept", |_| true);
        assert_eq!(mailbox.pending_len(), 1);
    }
}
