#![allow(dead_code, unused_imports, unused_mut, unused_variables)]

extern crate crossbeam;
extern crate ignore;
extern crate walkdir;

use std::env;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use crossbeam::sync::MsQueue;
use ignore::WalkBuilder;
use walkdir::WalkDir;

fn main() {
    let mut path = env::args().nth(1).unwrap();
    let mut parallel = false;
    let mut simple = false;
    // let count = Arc::new(AtomicUsize::new(0));
    let queue: Arc<MsQueue<Option<DirEntry>>> = Arc::new(MsQueue::new());
    if path == "parallel" {
        path = env::args().nth(2).unwrap();
        parallel = true;
    } else if path == "walkdir" {
        path = env::args().nth(2).unwrap();
        simple = true;
    }

    let stdout_queue = queue.clone();
    let stdout_thread = thread::spawn(move || {
        let mut stdout = io::BufWriter::new(io::stdout());
        while let Some(dent) = stdout_queue.pop() {
            write_path(&mut stdout, dent.path());
        }
    });

    if parallel {
        let walker = WalkBuilder::new(path).threads(6).build_parallel();
        let queue = queue.clone();
        // let count = count.clone();
        walker.run(move |result| {
            queue.push(Some(DirEntry::Y(result.unwrap())));
            // count.fetch_add(1, Ordering::SeqCst);
            // let stdout = io::stdout();
            // let mut stdout = stdout.lock();
            // write_path(&mut stdout, result.unwrap().path());
        });
    } else if simple {
        let mut stdout = io::BufWriter::new(io::stdout());
        let walker = WalkDir::new(path);
        for result in walker {
            queue.push(Some(DirEntry::X(result.unwrap())));
            // count.fetch_add(1, Ordering::SeqCst);
            // write_path(&mut stdout, result.unwrap().path());
        }
    } else {
        let mut stdout = io::BufWriter::new(io::stdout());
        let walker = WalkBuilder::new(path).build();
        for result in walker {
            queue.push(Some(DirEntry::Y(result.unwrap())));
            // count.fetch_add(1, Ordering::SeqCst);
            // write_path(&mut stdout, result.unwrap().path());
        }
    }
    queue.push(None);
    stdout_thread.join().unwrap();
    // println!("{}", count.load(Ordering::SeqCst));
}

enum DirEntry {
    X(walkdir::DirEntry),
    Y(ignore::DirEntry),
}

impl DirEntry {
    fn path(&self) -> &Path {
        match *self {
            DirEntry::X(ref x) => x.path(),
            DirEntry::Y(ref y) => y.path(),
        }
    }
}

fn write_path<W: Write>(mut wtr: W, path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    wtr.write(path.as_os_str().as_bytes()).unwrap();
    wtr.write(b"\n").unwrap();
}
