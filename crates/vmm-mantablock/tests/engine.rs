// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The ring backend against the real `mantablock-engine` process.
//!
//! Each test is ignored by default, because CI has no engine. Set
//! `MANTABLOCK_ENGINE` to the engine binary, built against the ring
//! commit the root `Cargo.toml` pins, and run with `--ignored`. A test
//! run that way fails without the variable rather than passes without
//! the engine.

#![allow(clippy::unwrap_used)]

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{kill_process, waitpid, Pid, Signal, WaitOptions};
use vmm_mantablock::{connect, Config, Job, Ring, Sink};
use vmm_storage::nvme::backend::{BlockOp, BlockStatus};

#[path = "engine/controller.rs"]
mod controller;

const PAGE: usize = 4096;
const BLOCKS: u64 = 4096;
const FILE_VOL: &str = "00000000-0000-0000-0000-00000000f11e";
const NULL_VOL: &str = "00000000-0000-0000-0000-0000000000ff";

/// One running engine with a file volume and a null volume.
struct Lab {
    engine: Child,
    _dirs: [tempfile::TempDir; 3],
    disk: tempfile::NamedTempFile,
    socks: PathBuf,
}

impl Lab {
    fn start() -> Self {
        let bin = std::env::var_os("MANTABLOCK_ENGINE")
            .expect("set MANTABLOCK_ENGINE to the mantablock-engine binary");
        let dirs = [
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
        ];
        let disk = tempfile::NamedTempFile::new().unwrap();
        disk.as_file().set_len(BLOCKS * PAGE as u64).unwrap();
        let engine = Command::new(bin)
            .arg("--socket-dir")
            .arg(dirs[0].path())
            .arg("--region-dir")
            .arg(dirs[1].path())
            .arg("--state-dir")
            .arg(dirs[2].path())
            .arg("--volume")
            .arg(format!(
                "{FILE_VOL},any,{BLOCKS},file:{}",
                disk.path().display()
            ))
            .arg("--volume")
            .arg(format!("{NULL_VOL},any,{BLOCKS},null"))
            .arg("--allow-null")
            .args(["--max-depth", "64", "--max-buf-pages", "512"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let socks = dirs[0].path().join("any");
        let lab = Self {
            engine,
            _dirs: dirs,
            disk,
            socks,
        };
        let until = Instant::now() + Duration::from_secs(10);
        while !lab.sock(FILE_VOL).exists() || !lab.sock(NULL_VOL).exists() {
            assert!(Instant::now() < until, "the engine made no sockets");
            thread::sleep(Duration::from_millis(10));
        }
        lab
    }

    fn sock(&self, vol: &str) -> PathBuf {
        self.socks.join(format!("{vol}.sock"))
    }

    fn config(&self, vol: &str) -> Config {
        let log = slog::Logger::root(slog::Discard, slog::o!());
        let mut c = Config::new(self.sock(vol), vol.parse().unwrap(), log);
        c.queues = 2;
        c.depth = 16;
        c.part_blocks = 16;
        c
    }

    fn pid(&self) -> Pid {
        Pid::from_raw(self.engine.id() as i32).unwrap()
    }

    /// Stops the engine, and returns once it is stopped: a signal is
    /// delivered later than `kill` returns, and a poller still running
    /// on another CPU would serve what the test sends next.
    fn stop(&self) {
        kill_process(self.pid(), Signal::STOP).unwrap();
        let (_, st) = waitpid(Some(self.pid()), WaitOptions::UNTRACED)
            .unwrap()
            .unwrap();
        assert!(st.stopped());
    }

    fn resume(&self) {
        kill_process(self.pid(), Signal::CONT).unwrap();
    }

    fn on_disk(&self, lba: u64, blocks: usize) -> Vec<u8> {
        let mut b = vec![0; blocks * PAGE];
        File::open(self.disk.path())
            .unwrap()
            .read_exact_at(&mut b, lba * PAGE as u64)
            .unwrap();
        b
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        let _ = self.engine.kill();
        let _ = self.engine.wait();
    }
}

struct TJob {
    id: u32,
    queue: u16,
    tag: u16,
    op: BlockOp,
    off: u64,
    mem: Arc<Mutex<Vec<u8>>>,
}

impl Job for TJob {
    type Id = u32;
    type Pages = ();

    fn id(&self) -> u32 {
        self.id
    }

    fn queue(&self) -> u16 {
        self.queue
    }

    fn tag(&self) -> u16 {
        self.tag
    }

    fn op(&self) -> BlockOp {
        self.op
    }

    fn byte_offset(&self) -> u64 {
        self.off
    }

    fn len(&self) -> usize {
        self.mem.lock().unwrap().len()
    }

    fn pages(&self) -> io::Result<()> {
        Ok(())
    }

    fn copy_from_guest(
        &self,
        _: &(),
        at: usize,
        dst: &mut [u8],
    ) -> io::Result<()> {
        dst.copy_from_slice(&self.mem.lock().unwrap()[at..at + dst.len()]);
        Ok(())
    }

    fn copy_to_guest(&self, _: &(), at: usize, src: &[u8]) -> io::Result<()> {
        self.mem.lock().unwrap()[at..at + src.len()].copy_from_slice(src);
        Ok(())
    }
}

#[derive(Default)]
struct Calls(Mutex<Vec<Vec<(u32, BlockStatus)>>>);

impl Sink<u32> for Calls {
    fn complete(&self, done: &[(u32, BlockStatus)]) {
        self.0.lock().unwrap().push(done.to_vec());
    }
}

impl Calls {
    fn all(&self) -> Vec<(u32, BlockStatus)> {
        self.0.lock().unwrap().iter().flatten().copied().collect()
    }

    fn wait_for(&self, n: usize, within: Duration) -> Vec<(u32, BlockStatus)> {
        let until = Instant::now() + within;
        loop {
            let all = self.all();
            if all.len() >= n {
                return all;
            }
            assert!(Instant::now() < until, "only {} of {n} done", all.len());
            thread::sleep(Duration::from_millis(1));
        }
    }
}

fn job(
    id: u32,
    queue: u16,
    tag: u16,
    op: BlockOp,
    lba: u64,
    data: Vec<u8>,
) -> TJob {
    TJob {
        id,
        queue,
        tag,
        op,
        off: lba * PAGE as u64,
        mem: Arc::new(Mutex::new(data)),
    }
}

fn pattern(blocks: usize, seed: u32) -> Vec<u8> {
    (0..blocks * PAGE)
        .map(|i| ((i as u32).wrapping_mul(31) ^ seed) as u8)
        .collect()
}

fn attach(_lab: &Lab, cfg: Config) -> (Arc<Ring<TJob>>, Arc<Calls>) {
    let calls = Arc::new(Calls::default());
    let ring = connect(cfg).unwrap().start(Arc::clone(&calls) as _);
    (ring, calls)
}

#[test]
#[ignore = "runs the real engine: set MANTABLOCK_ENGINE, pass --ignored"]
fn data_survives_the_ring_on_every_queue_and_size() {
    let lab = Lab::start();
    let (ring, calls) = attach(&lab, lab.config(FILE_VOL));
    assert!(ring.durable());
    assert_eq!(ring.capacity(), BLOCKS * PAGE as u64);
    assert_eq!(ring.queues(), 2);
    // 16 entries of 16 pages: 256 pages, under the cap of 512.
    assert_eq!(ring.queue_depth(), 16);

    // Up to MDTS, 512 blocks, which goes as 32 parts of 16.
    let sizes = [1usize, 3, 16, 17, 100, 512];
    let mut lba = 0u64;
    let mut want = Vec::new();
    for (i, &n) in sizes.iter().enumerate() {
        let q = 1 + (i % 2) as u16;
        let data = pattern(n, i as u32);
        ring.submit(job(
            i as u32,
            q,
            i as u16,
            BlockOp::Write { fua: true },
            lba,
            data.clone(),
        ));
        want.push((lba, data));
        lba += n as u64 + 5;
    }
    let done = calls.wait_for(sizes.len(), Duration::from_secs(20));
    assert!(
        done.iter().all(|(_, s)| *s == BlockStatus::Success),
        "{done:?}"
    );
    for (lba, data) in &want {
        assert_eq!(&lab.on_disk(*lba, data.len() / PAGE), data, "lba {lba}");
    }

    // And back through the ring into guest memory.
    let mut back = Vec::new();
    for (i, (lba, data)) in want.iter().enumerate() {
        let mem = Arc::new(Mutex::new(vec![0; data.len()]));
        let q = 2 - (i % 2) as u16;
        ring.submit(TJob {
            id: 100 + i as u32,
            queue: q,
            tag: i as u16,
            op: BlockOp::Read,
            off: lba * PAGE as u64,
            mem: Arc::clone(&mem),
        });
        back.push(mem);
    }
    let done = calls.wait_for(2 * sizes.len(), Duration::from_secs(20));
    assert!(done.iter().all(|(_, s)| *s == BlockStatus::Success));
    for ((_, data), mem) in want.iter().zip(&back) {
        assert_eq!(&*mem.lock().unwrap(), data);
    }
    ring.halt();
    assert!(!ring.is_lost());
}

#[test]
#[ignore = "runs the real engine: set MANTABLOCK_ENGINE, pass --ignored"]
fn completions_the_engine_publishes_together_reach_the_sink_together() {
    let lab = Lab::start();
    let (ring, calls) = attach(&lab, lab.config(FILE_VOL));
    // Stopped, the engine leaves all of them on the ring; resumed, its
    // poller takes them in one pass and publishes them at once.
    lab.stop();
    for t in 0..12u16 {
        ring.submit(job(
            u32::from(t),
            1,
            t,
            BlockOp::Read,
            u64::from(t),
            pattern(1, 0),
        ));
    }
    thread::sleep(Duration::from_millis(50));
    lab.resume();
    calls.wait_for(12, Duration::from_secs(5));
    let batches = calls.0.lock().unwrap().len();
    assert!(batches < 12, "{batches} sink calls for 12 completions");
    ring.halt();
}

#[test]
#[ignore = "runs the real engine: set MANTABLOCK_ENGINE, pass --ignored"]
fn a_killed_engine_fails_what_is_held_and_everything_after() {
    let mut lab = Lab::start();
    let (ring, calls) = attach(&lab, lab.config(FILE_VOL));
    lab.stop();
    for t in 0..4u16 {
        ring.submit(job(
            u32::from(t),
            1,
            t,
            BlockOp::Write { fua: false },
            0,
            pattern(2, 1),
        ));
    }
    lab.engine.kill().unwrap();
    lab.engine.wait().unwrap();
    let done = calls.wait_for(4, Duration::from_secs(5));
    assert!(
        done.iter().all(|(_, s)| *s == BlockStatus::Internal),
        "{done:?}"
    );
    assert!(ring.is_lost());
    ring.submit(job(9, 2, 0, BlockOp::Read, 0, pattern(1, 0)));
    assert_eq!(
        calls.wait_for(5, Duration::from_secs(1))[4],
        (9, BlockStatus::Internal)
    );
}

#[test]
#[ignore = "runs the real engine: set MANTABLOCK_ENGINE, pass --ignored"]
fn a_hung_engine_is_lost_after_the_heartbeat_limit() {
    let lab = Lab::start();
    let mut cfg = lab.config(FILE_VOL);
    cfg.heartbeat_limit = Duration::from_millis(300);
    let (ring, calls) = attach(&lab, cfg);
    lab.stop();
    ring.submit(job(1, 1, 0, BlockOp::Read, 0, pattern(1, 0)));
    let t = Instant::now();
    assert_eq!(
        calls.wait_for(1, Duration::from_secs(5)),
        vec![(1, BlockStatus::Internal)]
    );
    assert!(t.elapsed() >= Duration::from_millis(200));
    assert!(ring.is_lost());
    lab.resume();
}

#[test]
#[ignore = "runs the real engine: set MANTABLOCK_ENGINE, pass --ignored"]
fn a_volatile_engine_needs_consent_and_then_gets_flushes() {
    let lab = Lab::start();
    let err = connect(lab.config(NULL_VOL)).err().unwrap();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");

    let mut cfg = lab.config(NULL_VOL);
    cfg.allow_volatile = true;
    let (ring, calls) = attach(&lab, cfg);
    assert!(!ring.durable());
    ring.submit(job(1, 1, 0, BlockOp::Write { fua: true }, 0, pattern(4, 0)));
    ring.submit(job(2, 1, 1, BlockOp::Flush, 0, Vec::new()));
    let done = calls.wait_for(2, Duration::from_secs(5));
    assert!(
        done.iter().all(|(_, s)| *s == BlockStatus::Success),
        "{done:?}"
    );
    ring.sync().unwrap();
    ring.halt();
}

#[test]
#[ignore = "runs the real engine: set MANTABLOCK_ENGINE, pass --ignored"]
fn halt_detaches_so_the_volume_attaches_again() {
    let lab = Lab::start();
    let (ring, calls) = attach(&lab, lab.config(FILE_VOL));
    ring.submit(job(
        1,
        1,
        0,
        BlockOp::Write { fua: false },
        7,
        pattern(1, 7),
    ));
    calls.wait_for(1, Duration::from_secs(5));
    ring.halt();
    assert!(!ring.is_lost());
    // The engine allows one live attachment per volume; a second one
    // right away shows the first was detached.
    let (again, calls) = attach(&lab, lab.config(FILE_VOL));
    let mem = Arc::new(Mutex::new(vec![0; PAGE]));
    again.submit(TJob {
        id: 2,
        queue: 1,
        tag: 0,
        op: BlockOp::Read,
        off: 7 * PAGE as u64,
        mem: Arc::clone(&mem),
    });
    assert_eq!(
        calls.wait_for(1, Duration::from_secs(5)),
        vec![(2, BlockStatus::Success)]
    );
    assert_eq!(*mem.lock().unwrap(), pattern(1, 7));
    again.halt();
}

#[test]
#[ignore = "runs the real engine: set MANTABLOCK_ENGINE, pass --ignored"]
fn a_volume_the_engine_does_not_serve_is_refused() {
    let lab = Lab::start();
    let mut cfg = lab.config(FILE_VOL);
    cfg.socket = lab.sock(FILE_VOL);
    cfg.volume = NULL_VOL.parse().unwrap();
    let err = connect(cfg).err().unwrap();
    assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused, "{err}");
}
