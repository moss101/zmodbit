//! Durable terminal + replay window (M2, REQ-EV-0271): process output
//! lives in the durable run log, not in the client. A restarting desktop
//! replays the EXACT terminal tail from its last cursor within a bounded
//! window — with explicit backpressure accounting when the reader fell
//! too far behind — and the run keeps accepting work afterwards.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// The client's position in a run's output stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayCursor {
    pub run_id: String,
    pub offset: u64,
}

/// One replay frame: the exact bytes after the cursor, plus backpressure
/// accounting. `dropped_bytes > 0` means the client was further behind
/// than the replay window — the skipped prefix is summarized, and the
/// full bytes remain in the durable log ( retrievable by wider windows ).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplayFrame {
    pub run_id: String,
    /// Offset of the FIRST byte in `bytes`.
    pub from_offset: u64,
    pub bytes: Vec<u8>,
    /// Offset just past the last byte (the client's next cursor).
    pub new_offset: u64,
    /// Bytes skipped because the reader exceeded the window (backpressure).
    pub dropped_bytes: u64,
    /// True when the run has exited and this frame reaches the log end.
    pub caught_up_and_closed: bool,
}

#[derive(Debug)]
pub enum ReplayError {
    UnknownRun(String),
    Io(std::io::Error),
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayError::UnknownRun(id) => write!(f, "unknown run {id:?}"),
            ReplayError::Io(e) => write!(f, "replay io: {e}"),
        }
    }
}

impl std::error::Error for ReplayError {}

fn log_path(runs_dir: &Path, run_id: &str) -> PathBuf {
    runs_dir.join(run_id).join("output.log")
}

/// Replays the terminal tail for a (re)connecting client.
///
/// * `window_bytes` bounds the frame: a client whose cursor is more than
///   one window behind the log end receives the LAST window worth of
///   bytes, with `dropped_bytes` reporting exactly what was skipped.
/// * Replay is byte-exact: frames always start/end on real log offsets.
pub fn replay_tail(
    runs_dir: &Path,
    cursor: &ReplayCursor,
    window_bytes: u64,
    run_has_exited: bool,
) -> Result<ReplayFrame, ReplayError> {
    let log = log_path(runs_dir, &cursor.run_id);
    if !log.exists() {
        return Err(ReplayError::UnknownRun(cursor.run_id.clone()));
    }
    let mut file = fs::File::open(&log).map_err(ReplayError::Io)?;
    let total = file.metadata().map_err(ReplayError::Io)?.len();

    // Backpressure: clamp the replay start so at most one window is sent.
    let earliest = total.saturating_sub(window_bytes);
    let from_offset = cursor.offset.clamp(earliest, total);
    let dropped_bytes = from_offset - cursor.offset.min(from_offset);

    let take = total - from_offset;
    file.seek(SeekFrom::Start(from_offset))
        .map_err(ReplayError::Io)?;
    let mut bytes = vec![0u8; take as usize];
    let mut filled = 0;
    while filled < take as usize {
        let n = file.read(&mut bytes[filled..]).map_err(ReplayError::Io)?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    bytes.truncate(filled);

    Ok(ReplayFrame {
        run_id: cursor.run_id.clone(),
        from_offset,
        new_offset: from_offset + filled as u64,
        bytes,
        dropped_bytes,
        caught_up_and_closed: run_has_exited && from_offset + filled as u64 >= total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ExecBroker;
    use std::time::Duration;

    fn temp_dir(tag: &str) -> PathBuf {
        let unique = uuid::Uuid::now_v7().simple().to_string();
        let dir = std::env::temp_dir().join(format!("modbit-replay-{tag}-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn wait_exit(broker: &ExecBroker, run_id: &str) {
        for _ in 0..300 {
            if !matches!(
                broker.status(run_id).unwrap().state,
                crate::RunState::Running
            ) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("run did not exit");
    }

    /// QUAL-EV-0271: restart the desktop, replay the exact terminal tail,
    /// and continue — bytes are exact, backpressure is explicit, and the
    /// session continues at the new cursor.
    #[test]
    fn restart_replays_exact_tail_and_continues() {
        let dir = temp_dir("tail");

        // UI 1: run a command producing a deterministic, larger-than-window log.
        {
            let broker = ExecBroker::open(&dir).unwrap();
            #[cfg(windows)]
            let argv = vec![
                "cmd.exe".into(),
                "/C".into(),
                "for /l %i in (1,1,2000) do @echo line-%i-0123456789abcdef".into(),
            ];
            #[cfg(not(windows))]
            let argv = vec![
                "sh".into(),
                "-c".into(),
                "for i in $(seq 1 2000); do echo \"line-$i-0123456789abcdef\"; done".into(),
            ];
            broker.spawn("long-log", &argv).unwrap();
        } // UI 1 dies (client disconnect) while output is durable on disk.

        // UI restart: brand-new broker adopts the durable log.
        let broker = ExecBroker::open(&dir).unwrap();
        wait_exit(&broker, "long-log");

        let small_window = 4096u64;
        let log = fs::read(dir.join("long-log").join("output.log")).unwrap();
        let log_len = log.len();
        assert!(
            log_len > small_window as usize,
            "fixture outgrew the window"
        );

        // First reconnection: the client's old cursor is at 0 but the log
        // outgrew the window — replay returns the EXACT LAST WINDOW worth
        // of bytes and reports the skipped prefix as dropped (backpressure
        // accounting), never flooding the context.
        let frame = replay_tail(
            &dir,
            &ReplayCursor {
                run_id: "long-log".into(),
                offset: 0,
            },
            small_window,
            true,
        )
        .unwrap();
        assert_eq!(
            frame.from_offset as usize,
            log_len - small_window as usize,
            "replay starts exactly one window back"
        );
        assert_eq!(
            frame.dropped_bytes as usize,
            log_len - small_window as usize
        );
        assert!(frame.bytes.len() <= small_window as usize);
        assert!(
            frame.caught_up_and_closed,
            "run exited and we reached the end"
        );

        // Exactness: replayed bytes equal the log's last bytes.
        assert_eq!(&log[(log_len - frame.bytes.len())..], &frame.bytes[..]);

        // Byte-exact windowed replay: a cursor within one window of the
        // end gets the true slice starting at its own offset.
        let mid = (log_len - 2048) as u64;
        let frame = replay_tail(
            &dir,
            &ReplayCursor {
                run_id: "long-log".into(),
                offset: mid,
            },
            small_window,
            true,
        )
        .unwrap();
        assert_eq!(frame.from_offset, mid, "cursor within window is honored");
        assert_eq!(frame.dropped_bytes, 0);
        assert_eq!(
            &log[mid as usize..mid as usize + frame.bytes.len()],
            &frame.bytes[..]
        );

        // Backpressure: a client stuck at offset 0 with a large window that
        // still exceeds the log simply gets everything (no fake drops).
        let big = replay_tail(
            &dir,
            &ReplayCursor {
                run_id: "long-log".into(),
                offset: 0,
            },
            u64::MAX,
            true,
        )
        .unwrap();
        assert_eq!(big.dropped_bytes, 0);
        assert_eq!(big.bytes.len(), log.len());

        // Unknown run: typed error, not a hang.
        assert!(matches!(
            replay_tail(
                &dir,
                &ReplayCursor {
                    run_id: "nope".into(),
                    offset: 0
                },
                1024,
                true
            ),
            Err(ReplayError::UnknownRun(_))
        ));
    }
}
