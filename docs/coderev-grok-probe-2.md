# CodeRev Grok probe (second)

The pinned action was the blocker: workflows pinned coderev@b86b8a2, which
predated both the wrapper fix and the zero-findings diagnostic, so neither
had ever executed in CI. Pins are now bumped to 90e7f0e.

This probe carries a distinct diff so the content hash differs from the first
one and the unchanged-diff skip does not fire.
