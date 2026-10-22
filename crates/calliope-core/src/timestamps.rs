// per-frame pts of a g2g decode against ffprobe's -show_frames for the same stream

use std::collections::HashMap;

use crate::{Error, Result};

const NANOSECONDS_PER_SECOND: i128 = 1_000_000_000;

// a stream time base, numerator / denominator seconds per tick
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeBase {
    pub numerator: u64,
    pub denominator: u64,
}

impl TimeBase {
    // parse ffprobe's `1/90000` form
    pub fn parse(text: &str) -> Result<Self> {
        let bad = || Error::Parse(format!("not a time base: {text:?}"));
        let (numerator, denominator) = text.split_once('/').ok_or_else(bad)?;
        let time_base = Self {
            numerator: numerator.trim().parse().map_err(|_| bad())?,
            denominator: denominator.trim().parse().map_err(|_| bad())?,
        };
        if time_base.numerator == 0 || time_base.denominator == 0 {
            return Err(bad());
        }
        Ok(time_base)
    }

    // the tick nearest to `nanoseconds`, rounding half up
    pub fn nearest_tick(self, nanoseconds: u64) -> i128 {
        let tick_ns_times_denominator = i128::from(self.numerator) * NANOSECONDS_PER_SECOND;
        (2 * i128::from(nanoseconds) * i128::from(self.denominator) + tick_ns_times_denominator)
            / (2 * tick_ns_times_denominator)
    }

    pub fn seconds(self, ticks: i128) -> f64 {
        ticks as f64 * self.numerator as f64 / self.denominator as f64
    }
}

// one decoded frame as ffprobe reports it, in decoder output order
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceFrame {
    // the pts the frame carried out of the decoder, None when its packet had none
    pub pts: Option<i64>,
    // the pts ffmpeg stamps on the frame: `pts` when set, else a guess from the packet dts
    pub best_effort_timestamp: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceTimestamps {
    pub time_base: TimeBase,
    pub frames: Vec<ReferenceFrame>,
}

// which frames pair by output index with ffprobe's
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FramePairing {
    EveryFrame,
    // PCM frames are whatever chunks the demuxer reads, so only the first lines up
    FirstFrame,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimestampVerdict {
    Match,
    PtsDiffers {
        frame: usize,
        g2g_ns: Option<u64>,
        reference_ticks: i64,
        // ffmpeg stamped this frame with a guess, the frame had no pts or one out of order
        reference_guessed: bool,
    },
    // every paired frame matched, one side has more frames
    FrameCountDiffers {
        frames: usize,
        reference_frames: usize,
    },
    // a frame no reference pts checked does not come after the one before it
    NotIncreasing {
        frame: usize,
    },
    // ffprobe gave no frame a timestamp and g2g's increase
    NoReferenceTimestamps,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimestampComparison {
    // the most common g2g tick minus the frame's own ffprobe tick over the paired frames, taken off every frame before comparing
    pub offset_ticks: i128,
    pub verdict: TimestampVerdict,
}

pub fn compare(
    reference: &ReferenceTimestamps,
    g2g_pts_ns: &[Option<u64>],
    pairing: FramePairing,
) -> TimestampComparison {
    let paired_frames = match pairing {
        FramePairing::EveryFrame => g2g_pts_ns.len(),
        FramePairing::FirstFrame => 1,
    };
    let time_base = reference.time_base;
    let pairs: Vec<(usize, &ReferenceFrame, Option<u64>)> = reference
        .frames
        .iter()
        .zip(g2g_pts_ns)
        .take(paired_frames)
        .enumerate()
        .map(|(frame, (reference_frame, &g2g_ns))| (frame, reference_frame, g2g_ns))
        .collect();
    let differences: Vec<i128> = pairs
        .iter()
        .filter_map(|(_, reference_frame, g2g_ns)| {
            let reference_ticks = reference_frame
                .pts
                .or(reference_frame.best_effort_timestamp)?;
            Some(time_base.nearest_tick((*g2g_ns)?) - i128::from(reference_ticks))
        })
        .collect();
    let offset_ticks = most_common(&differences).unwrap_or(0);
    let compared = |verdict| TimestampComparison {
        offset_ticks,
        verdict,
    };
    let mut checked = vec![false; g2g_pts_ns.len()];
    for (frame, reference_frame, g2g_ns) in pairs {
        let Some(reference_ticks) = reference_frame.best_effort_timestamp else {
            continue;
        };
        let reference_guessed = reference_frame.pts != Some(reference_ticks);
        // where ffmpeg replaced a frame's own pts, carrying the decoder's pts unchanged matches too
        let accepted_ticks = [
            Some(reference_ticks),
            reference_frame.pts.filter(|_| reference_guessed),
        ];
        let matched = g2g_ns.is_some_and(|ns| {
            let g2g_ticks = time_base.nearest_tick(ns) - offset_ticks;
            accepted_ticks
                .iter()
                .flatten()
                .any(|&ticks| g2g_ticks == i128::from(ticks))
        });
        if !matched {
            return compared(TimestampVerdict::PtsDiffers {
                frame,
                g2g_ns,
                reference_ticks,
                reference_guessed,
            });
        }
        checked[frame] = true;
    }
    if pairing == FramePairing::EveryFrame && reference.frames.len() != g2g_pts_ns.len() {
        return compared(TimestampVerdict::FrameCountDiffers {
            frames: g2g_pts_ns.len(),
            reference_frames: reference.frames.len(),
        });
    }
    let not_increasing = (1..g2g_pts_ns.len()).find(|&frame| {
        !checked[frame]
            && match (g2g_pts_ns[frame - 1], g2g_pts_ns[frame]) {
                (Some(previous), Some(next)) => next <= previous,
                _ => true,
            }
    });
    if let Some(frame) = not_increasing {
        return compared(TimestampVerdict::NotIncreasing { frame });
    }
    if !checked.contains(&true) {
        return compared(TimestampVerdict::NoReferenceTimestamps);
    }
    compared(TimestampVerdict::Match)
}

// the value seen most often, the earliest of a tie
fn most_common(values: &[i128]) -> Option<i128> {
    let mut counts: HashMap<i128, usize> = HashMap::new();
    for value in values {
        *counts.entry(*value).or_default() += 1;
    }
    let highest = counts.values().copied().max()?;
    values
        .iter()
        .copied()
        .find(|value| counts[value] == highest)
}

// two decodes' pts agree when they have as many frames and each pair rounds to the same tick
pub fn same_ticks(time_base: TimeBase, left: &[Option<u64>], right: &[Option<u64>]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(&left, &right)| match (left, right) {
                (Some(left), Some(right)) => {
                    time_base.nearest_tick(left) == time_base.nearest_tick(right)
                }
                (None, None) => true,
                _ => false,
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MPEG_TS: TimeBase = TimeBase {
        numerator: 1,
        denominator: 90_000,
    };

    fn stamped(ticks: &[i64]) -> ReferenceTimestamps {
        ReferenceTimestamps {
            time_base: MPEG_TS,
            frames: ticks
                .iter()
                .map(|&t| ReferenceFrame {
                    pts: Some(t),
                    best_effort_timestamp: Some(t),
                })
                .collect(),
        }
    }

    // nanoseconds for `ticks` at 1/90000, truncated the way g2g converts
    fn truncated_ns(ticks: i64) -> Option<u64> {
        Some((i128::from(ticks) * NANOSECONDS_PER_SECOND / 90_000) as u64)
    }

    #[test]
    fn parses_and_rounds_to_the_nearest_tick() {
        assert_eq!(TimeBase::parse("1/90000").unwrap(), MPEG_TS);
        assert!(TimeBase::parse("0/1").is_err());
        assert!(TimeBase::parse("90000").is_err());
        // 132000 ticks is 1466666666.67 ns, g2g truncates it
        assert_eq!(MPEG_TS.nearest_tick(1_466_666_666), 132_000);
        let one_tick_ns = 1_000_000_000 / 90_000;
        assert_eq!(MPEG_TS.nearest_tick(1_466_666_666 + one_tick_ns), 132_001);
    }

    fn verdict(
        reference: &ReferenceTimestamps,
        g2g: &[Option<u64>],
        pairing: FramePairing,
    ) -> TimestampVerdict {
        compare(reference, g2g, pairing).verdict
    }

    #[test]
    fn truncated_nanoseconds_match_their_tick() {
        let ticks = [126_000, 132_000, 138_000];
        let g2g: Vec<Option<u64>> = ticks.iter().map(|&t| truncated_ns(t)).collect();
        assert_eq!(
            compare(&stamped(&ticks), &g2g, FramePairing::EveryFrame),
            TimestampComparison {
                offset_ticks: 0,
                verdict: TimestampVerdict::Match
            }
        );
    }

    #[test]
    fn a_shift_shared_by_every_frame_is_an_offset() {
        let ticks = [0, 3000, 6000];
        let shift = 1024;
        let g2g: Vec<Option<u64>> = ticks.iter().map(|&t| truncated_ns(t + shift)).collect();
        assert_eq!(
            compare(&stamped(&ticks), &g2g, FramePairing::EveryFrame),
            TimestampComparison {
                offset_ticks: i128::from(shift),
                verdict: TimestampVerdict::Match
            }
        );
    }

    #[test]
    fn one_odd_first_frame_does_not_set_the_offset() {
        // ffmpeg trims an mp3's encoder delay and stamps its partial first frame later
        let ticks = [2255, 3000, 6000, 9000];
        let g2g = [
            truncated_ns(0),
            truncated_ns(3000),
            truncated_ns(6000),
            truncated_ns(9000),
        ];
        assert_eq!(
            compare(&stamped(&ticks), &g2g, FramePairing::EveryFrame),
            TimestampComparison {
                offset_ticks: 0,
                verdict: TimestampVerdict::PtsDiffers {
                    frame: 0,
                    g2g_ns: g2g[0],
                    reference_ticks: 2255,
                    reference_guessed: false,
                }
            }
        );
    }

    #[test]
    fn names_the_first_frame_that_differs_after_the_offset() {
        let ticks = [126_000, 132_000, 138_000];
        let mut g2g: Vec<Option<u64>> = ticks.iter().map(|&t| truncated_ns(t)).collect();
        g2g[1] = truncated_ns(132_000 + 3000);
        assert_eq!(
            verdict(&stamped(&ticks), &g2g, FramePairing::EveryFrame),
            TimestampVerdict::PtsDiffers {
                frame: 1,
                g2g_ns: g2g[1],
                reference_ticks: 132_000,
                reference_guessed: false,
            }
        );
        g2g[1] = None;
        assert!(matches!(
            verdict(&stamped(&ticks), &g2g, FramePairing::EveryFrame),
            TimestampVerdict::PtsDiffers {
                frame: 1,
                g2g_ns: None,
                ..
            }
        ));
    }

    #[test]
    fn the_decoders_own_pts_matches_where_ffmpeg_replaced_it() {
        // flv/Enigma_Principles_of_Lust-part.flv: the fourth frame leaves the decoder at 33 ms, ffmpeg restamps it 133
        let mut reference = stamped(&[0, 67, 100, 133]);
        reference.frames[3].pts = Some(33);
        let millisecond = TimeBase::parse("1/1000").unwrap();
        reference.time_base = millisecond;
        let at = |ms: u64| Some(ms * 1_000_000);
        assert_eq!(
            verdict(
                &reference,
                &[at(0), at(67), at(100), at(33)],
                FramePairing::EveryFrame
            ),
            TimestampVerdict::Match
        );
        assert!(matches!(
            verdict(
                &reference,
                &[at(0), at(67), at(100), at(50)],
                FramePairing::EveryFrame
            ),
            TimestampVerdict::PtsDiffers {
                frame: 3,
                reference_guessed: true,
                ..
            }
        ));
    }

    #[test]
    fn the_offset_comes_from_the_decoders_own_pts() {
        // ffmpeg restamps three of four frames, g2g carries the decoder's pts on all of them
        let mut reference = stamped(&[0, 3000, 6000, 9000]);
        let decoder_ticks = [0, 6000, 9000, 12000];
        for (frame, ticks) in reference.frames.iter_mut().zip(decoder_ticks) {
            frame.pts = Some(ticks);
        }
        let g2g: Vec<Option<u64>> = decoder_ticks.iter().map(|&t| truncated_ns(t)).collect();
        assert_eq!(
            compare(&reference, &g2g, FramePairing::EveryFrame),
            TimestampComparison {
                offset_ticks: 0,
                verdict: TimestampVerdict::Match
            }
        );
    }

    #[test]
    fn a_guessed_reference_pts_is_flagged() {
        let mut reference = stamped(&[0, 3000]);
        reference.frames[1].pts = None;
        assert!(matches!(
            verdict(&reference, &[Some(0), Some(0)], FramePairing::EveryFrame),
            TimestampVerdict::PtsDiffers {
                frame: 1,
                reference_guessed: true,
                ..
            }
        ));
    }

    #[test]
    fn extra_frames_after_a_matching_prefix_are_a_count_difference() {
        let ticks = [0, 3000];
        let g2g = [truncated_ns(0), truncated_ns(3000), truncated_ns(6000)];
        assert_eq!(
            verdict(&stamped(&ticks), &g2g, FramePairing::EveryFrame),
            TimestampVerdict::FrameCountDiffers {
                frames: 3,
                reference_frames: 2,
            }
        );
    }

    #[test]
    fn first_frame_pairing_checks_the_start_then_that_pts_increase() {
        // four PCM packets as ffprobe reads them, against g2g's two larger buffers
        let reference = stamped(&[0, 1000, 2000, 3000]);
        let larger_buffers = [truncated_ns(0), truncated_ns(2000)];
        assert_eq!(
            compare(&reference, &larger_buffers, FramePairing::FirstFrame),
            TimestampComparison {
                offset_ticks: 0,
                verdict: TimestampVerdict::Match
            }
        );
        let late_start = [truncated_ns(500), truncated_ns(2000)];
        assert_eq!(
            compare(&reference, &late_start, FramePairing::FirstFrame).offset_ticks,
            500
        );
        let stuck = [truncated_ns(0), truncated_ns(0), truncated_ns(0)];
        assert_eq!(
            verdict(&reference, &stuck, FramePairing::FirstFrame),
            TimestampVerdict::NotIncreasing { frame: 1 }
        );
    }

    #[test]
    fn without_reference_timestamps_g2g_must_increase() {
        let unstamped = ReferenceTimestamps {
            time_base: MPEG_TS,
            frames: vec![
                ReferenceFrame {
                    pts: None,
                    best_effort_timestamp: None,
                };
                3
            ],
        };
        let increasing = [Some(0), Some(40), Some(80)];
        assert_eq!(
            verdict(&unstamped, &increasing, FramePairing::EveryFrame),
            TimestampVerdict::NoReferenceTimestamps
        );
        assert_eq!(
            verdict(
                &unstamped,
                &[Some(0), Some(0), Some(0)],
                FramePairing::EveryFrame
            ),
            TimestampVerdict::NotIncreasing { frame: 1 }
        );
    }

    #[test]
    fn two_decodes_agree_tick_for_tick() {
        // gstreamer rounds 1/44100 ticks down where g2g rounds them up
        let audio = TimeBase::parse("1/44100").unwrap();
        let gstreamer = [Some(23_219_954), Some(46_439_909)];
        let g2g = [Some(23_219_955), Some(46_439_909)];
        assert!(same_ticks(audio, &gstreamer, &g2g));
        assert!(!same_ticks(audio, &gstreamer, &g2g[..1]));
        assert!(!same_ticks(audio, &gstreamer, &[Some(0), Some(46_439_909)]));
    }
}
