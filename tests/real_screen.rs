//! By hand only, on a Mac (`cargo test --release --test real_screen -- --ignored --nocapture`): the real
//! screen as the watchdog reads it. It prints the displays, how long a take and its grid take, and how
//! much changed between two takes a second apart, then deletes the takes. Nothing is posted.

use std::time::{Duration, Instant};

use obs::frames::Grid;
use obs::screen::{self, Real};

#[test]
#[ignore]
fn the_real_screen() {
    let pid = screen::obs_pid();
    let s = screen::look(pid).unwrap();
    println!("OBS process: {pid:?}; locked: {}; OBS windows on screen: {}", s.locked, s.obs_windows.len());
    for d in &s.displays {
        println!("display {} {} main={} asleep={} bounds={:?}", d.id, d.uuid, d.main, d.asleep, d.bounds);
    }
    let main = screen::find(&s, None, None, None).expect("a main display");
    let dir = tempfile::tempdir().unwrap();
    let mut grids = Vec::new();
    for i in 0..2 {
        let file = dir.path().join(format!("take{i}.png"));
        let t = Instant::now();
        screen::take(&Real::Display(main.clone()), &file).unwrap();
        let took = t.elapsed();
        let bytes = std::fs::read(&file).unwrap();
        let t = Instant::now();
        let grid = Grid::from_png(&bytes).unwrap();
        let (w, h) = png_size(&bytes);
        println!(
            "take {i}: {w}x{h} px, {} KB, screencapture {took:?}, grid {:?} ({}x{})",
            bytes.len() / 1000,
            t.elapsed(),
            grid.width,
            grid.height
        );
        grids.push(grid);
        std::thread::sleep(Duration::from_secs(1));
    }
    let masks = screen::masks(&main, &s.obs_windows);
    let change = grids[1].changed(&grids[0], 12, &masks);
    println!(
        "changed between takes: {} of {} cells ({:.4}), {} OBS windows masked",
        change.cells,
        change.of,
        change.fraction(),
        masks.len()
    );
}

fn png_size(b: &[u8]) -> (u32, u32) {
    (u32::from_be_bytes(b[16..20].try_into().unwrap()), u32::from_be_bytes(b[20..24].try_into().unwrap()))
}
