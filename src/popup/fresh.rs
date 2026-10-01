//! Which snapshots the popup may apply after its own write. Pure: no I/O,
//! the clock is passed in.
//!
//! After a write (rename, close, new window, …) the popup re-reads tmux at
//! once (through the daemon's `refresh` op when live, else directly) and
//! applies that read. A snapshot read *before* the write can still arrive
//! afterwards (a push already in flight, a degraded read that started
//! earlier); applying it would undo the write on screen until the next
//! change. The read the popup applied sets a `Floor`; anything older than
//! the floor is dropped. This is the one staleness guard: the daemon's fresh
//! read on subscribe and the popup's wanted selection (`App::want`) only
//! make the first picture right and keep the cursor put.

use std::time::{Duration, Instant};

/// A floor stops guarding after this long, so nothing (a clock oddity, an
/// unforeseen ordering) can freeze the list.
pub const FLOOR_TTL: Duration = Duration::from_secs(2);

/// Where a snapshot came from, and so how old it can be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stamp {
    /// From daemon instance `epoch`, published as `seq`, over a connection
    /// the popup opened at `since` (the daemon reads tmux afresh for a new
    /// subscription and for `refresh`, so everything it sends over that
    /// connection was read after `since`).
    Live {
        epoch: u64,
        seq: u64,
        since: Instant,
    },
    /// A direct read of tmux that started at `at`.
    Direct { at: Instant },
}

/// The read applied right after the popup's own write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Floor {
    /// When that read was requested (after the write).
    pub at: Instant,
    /// Its daemon instance and seq, if it came from the daemon.
    pub live: Option<(u64, u64)>,
}

impl Floor {
    pub fn of(stamp: Stamp) -> Floor {
        match stamp {
            Stamp::Live { epoch, seq, since } => Floor {
                at: since,
                live: Some((epoch, seq)),
            },
            Stamp::Direct { at } => Floor { at, live: None },
        }
    }
}

/// Whether a snapshot stamped `s` is at least as new as `floor`.
pub fn accept(floor: Option<&Floor>, s: &Stamp, now: Instant) -> bool {
    let Some(f) = floor else { return true };
    if now.saturating_duration_since(f.at) >= FLOOR_TTL {
        return true;
    }
    match *s {
        Stamp::Direct { at } => at >= f.at,
        // the same daemon numbers its snapshots in order
        Stamp::Live { epoch, seq, .. } if f.live.is_some_and(|(e, _)| e == epoch) => {
            f.live.is_some_and(|(_, fs)| seq >= fs)
        }
        // another daemon, or a floor from a direct read: only a
        // connection opened after the floor's read is surely newer
        Stamp::Live { since, .. } => since >= f.at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(ms: u64) -> Instant {
        thread_local!(static T0: Instant = Instant::now());
        T0.with(|t0| *t0 + Duration::from_millis(ms))
    }

    fn live(epoch: u64, seq: u64, since: u64) -> Stamp {
        Stamp::Live {
            epoch,
            seq,
            since: t(since),
        }
    }

    #[test]
    fn no_floor_accepts_everything() {
        assert!(accept(None, &live(1, 1, 0), t(0)));
        assert!(accept(None, &Stamp::Direct { at: t(0) }, t(0)));
    }

    /// The pass 2 rule: a degraded read that started before the popup's
    /// own read is dropped; one that started after it is applied.
    #[test]
    fn direct_reads_compare_start_times() {
        let f = Floor::of(Stamp::Direct { at: t(100) });
        assert!(!accept(Some(&f), &Stamp::Direct { at: t(99) }, t(150)));
        assert!(accept(Some(&f), &Stamp::Direct { at: t(100) }, t(150)));
        assert!(accept(Some(&f), &Stamp::Direct { at: t(101) }, t(150)));
    }

    /// After a `refresh` answered with seq 7, a push of seq 6 still in
    /// flight on the subscription is older than the write: dropped.
    #[test]
    fn same_daemon_compares_seqs() {
        let f = Floor::of(live(1, 7, 100));
        assert!(!accept(Some(&f), &live(1, 6, 0), t(150)));
        assert!(accept(Some(&f), &live(1, 7, 0), t(150)));
        assert!(accept(Some(&f), &live(1, 8, 0), t(150)));
        // a degraded read that started before the refresh is older too
        assert!(!accept(Some(&f), &Stamp::Direct { at: t(90) }, t(150)));
        assert!(accept(Some(&f), &Stamp::Direct { at: t(110) }, t(150)));
    }

    /// Seqs of different daemon instances don't compare (a replacement
    /// counts from 1): a connection opened after the floor is newer, the
    /// old subscription's buffered pushes are not.
    #[test]
    fn another_daemon_or_a_direct_floor_compares_connection_times() {
        let f = Floor::of(live(1, 50, 100));
        assert!(!accept(Some(&f), &live(2, 1, 90), t(150)));
        assert!(accept(Some(&f), &live(2, 1, 120), t(150)));
        let f = Floor::of(Stamp::Direct { at: t(100) });
        assert!(!accept(Some(&f), &live(1, 9, 90), t(150)));
        assert!(accept(Some(&f), &live(1, 1, 100), t(150)));
    }

    #[test]
    fn a_floor_expires() {
        let f = Floor::of(live(1, 7, 100));
        let old = live(1, 6, 0);
        assert!(!accept(
            Some(&f),
            &old,
            t(100) + FLOOR_TTL - Duration::from_millis(1)
        ));
        assert!(accept(Some(&f), &old, t(100) + FLOOR_TTL));
    }
}
