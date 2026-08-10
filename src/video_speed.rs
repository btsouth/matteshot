//! Source-time ranges that are compressed during preview and export.
//!
//! The editor stays in source time so trims and annotations remain attached to
//! the recorded moments. `TimeMap` is the one place that translates those
//! moments into the shorter output timeline.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpeedRange {
    pub start: i64,
    pub end: i64,
    pub rate: u32,
}

impl SpeedRange {
    pub fn new(start: i64, end: i64, rate: u32) -> Self {
        Self { start, end, rate }
    }
}

#[derive(Clone, Debug)]
pub struct TimeMap {
    start: i64,
    end: i64,
    ranges: Vec<SpeedRange>,
}

impl TimeMap {
    pub fn new(start: i64, end: i64, ranges: &[SpeedRange]) -> Result<Self, &'static str> {
        if end <= start {
            return Err("the selected video range is empty");
        }
        let mut clipped: Vec<_> = ranges
            .iter()
            .filter_map(|range| {
                let start = range.start.max(start);
                let end = range.end.min(end);
                (end > start).then_some(SpeedRange::new(start, end, range.rate))
            })
            .collect();
        clipped.sort_by_key(|range| (range.start, range.end));

        let mut previous_end = start;
        for range in &clipped {
            if !matches!(range.rate, 2 | 4 | 8 | 16) {
                return Err("speed must be 2x, 4x, 8x, or 16x");
            }
            if range.start < previous_end {
                return Err("speed sections cannot overlap");
            }
            previous_end = range.end;
        }

        Ok(Self { start, end, ranges: clipped })
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub fn output_time(&self, source_time: i64) -> i64 {
        let target = source_time.clamp(self.start, self.end);
        let mut source_cursor = self.start;
        let mut output = 0i64;
        for range in &self.ranges {
            if target <= range.start {
                break;
            }
            output += range.start.saturating_sub(source_cursor);
            let sped_end = target.min(range.end);
            output += sped_end.saturating_sub(range.start) / range.rate as i64;
            if target <= range.end {
                return output;
            }
            source_cursor = range.end;
        }
        output + target.saturating_sub(source_cursor)
    }

    pub fn output_duration(&self) -> i64 {
        self.output_time(self.end)
    }

    pub fn rate_at(&self, source_time: i64) -> u32 {
        self.ranges
            .iter()
            .find(|range| source_time >= range.start && source_time < range.end)
            .map_or(1, |range| range.rate)
    }

    /// Contiguous source-time pieces annotated with their playback rate.
    pub fn segments(&self, from: i64, to: i64) -> Vec<(i64, i64, u32)> {
        let from = from.clamp(self.start, self.end);
        let to = to.clamp(self.start, self.end);
        if to <= from {
            return Vec::new();
        }
        let mut points = vec![from, to];
        for range in &self.ranges {
            if range.start > from && range.start < to {
                points.push(range.start);
            }
            if range.end > from && range.end < to {
                points.push(range.end);
            }
        }
        points.sort_unstable();
        points.dedup();
        points
            .windows(2)
            .map(|pair| (pair[0], pair[1], self.rate_at(pair[0])))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{SpeedRange, TimeMap};

    const SECOND: i64 = 10_000_000;

    #[test]
    fn compresses_multiple_sections_while_preserving_source_time() {
        let map = TimeMap::new(
            0,
            60 * SECOND,
            &[
                SpeedRange::new(10 * SECOND, 30 * SECOND, 4),
                SpeedRange::new(40 * SECOND, 56 * SECOND, 8),
            ],
        )
        .unwrap();

        assert_eq!(map.output_time(10 * SECOND), 10 * SECOND);
        assert_eq!(map.output_time(30 * SECOND), 15 * SECOND);
        assert_eq!(map.output_time(40 * SECOND), 25 * SECOND);
        assert_eq!(map.output_duration(), 31 * SECOND);
    }

    #[test]
    fn clips_sections_to_the_trim_without_mutating_source_ranges() {
        let map = TimeMap::new(
            10 * SECOND,
            30 * SECOND,
            &[SpeedRange::new(0, 20 * SECOND, 2)],
        )
        .unwrap();

        assert_eq!(map.output_time(10 * SECOND), 0);
        assert_eq!(map.output_time(20 * SECOND), 5 * SECOND);
        assert_eq!(map.output_duration(), 15 * SECOND);
    }

    #[test]
    fn exposes_normal_and_sped_segments_for_audio_retiming() {
        let map = TimeMap::new(
            0,
            10 * SECOND,
            &[SpeedRange::new(2 * SECOND, 6 * SECOND, 4)],
        )
        .unwrap();

        assert_eq!(
            map.segments(SECOND, 8 * SECOND),
            vec![
                (SECOND, 2 * SECOND, 1),
                (2 * SECOND, 6 * SECOND, 4),
                (6 * SECOND, 8 * SECOND, 1),
            ]
        );
    }

    #[test]
    fn rejects_overlaps_and_unsupported_rates() {
        assert!(TimeMap::new(
            0,
            10 * SECOND,
            &[
                SpeedRange::new(SECOND, 5 * SECOND, 4),
                SpeedRange::new(4 * SECOND, 7 * SECOND, 8),
            ],
        )
        .is_err());
        assert!(TimeMap::new(
            0,
            10 * SECOND,
            &[SpeedRange::new(SECOND, 2 * SECOND, 3)],
        )
        .is_err());
    }
}
