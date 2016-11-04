use std::ffi::OsStr;
use std::fs::{self, FileType, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::vec;

use crossbeam::sync::MsQueue;
use walkdir::{self, WalkDir, WalkDirIterator};

use dir::{Ignore, IgnoreBuilder};
use gitignore::GitignoreBuilder;
use overrides::Override;
use types::Types;
use {Error, PartialErrorBuilder};

/// A directory entry with a possible error attached.
///
/// The error typically refers to a problem parsing ignore files in a
/// particular directory.
#[derive(Debug)]
pub struct DirEntry {
    dent: DirEntryInner,
    err: Option<Error>,
}

impl DirEntry {
    /// The full path that this entry represents.
    pub fn path(&self) -> &Path {
        self.dent.path()
    }

    /// Whether this entry corresponds to a symbolic link or not.
    pub fn path_is_symbolic_link(&self) -> bool {
        self.dent.path_is_symbolic_link()
    }

    /// Returns true if and only if this entry corresponds to stdin.
    ///
    /// i.e., The entry has depth 0 and its file name is `-`.
    pub fn is_stdin(&self) -> bool {
        self.dent.is_stdin()
    }

    /// Return the metadata for the file that this entry points to.
    pub fn metadata(&self) -> Result<Metadata, Error> {
        self.dent.metadata()
    }

    /// Return the file type for the file that this entry points to.
    ///
    /// This entry doesn't have a file type if it corresponds to stdin.
    pub fn file_type(&self) -> Option<FileType> {
        self.dent.file_type()
    }

    /// Return the file name of this entry.
    ///
    /// If this entry has no file name (e.g., `/`), then the full path is
    /// returned.
    pub fn file_name(&self) -> &OsStr {
        self.dent.file_name()
    }

    /// Returns the depth at which this entry was created relative to the root.
    pub fn depth(&self) -> usize {
        self.dent.depth()
    }

    /// Returns an error, if one exists, associated with processing this entry.
    ///
    /// An example of an error is one that occurred while parsing an ignore
    /// file.
    pub fn error(&self) -> Option<&Error> {
        self.err.as_ref()
    }

    fn new_stdin() -> DirEntry {
        DirEntry {
            dent: DirEntryInner::Stdin,
            err: None,
        }
    }

    fn new_walkdir(dent: walkdir::DirEntry, err: Option<Error>) -> DirEntry {
        DirEntry {
            dent: DirEntryInner::Walkdir(dent),
            err: err,
        }
    }

    fn new_raw(dent: DirEntryRaw, err: Option<Error>) -> DirEntry {
        DirEntry {
            dent: DirEntryInner::Raw(dent),
            err: err,
        }
    }
}

/// DirEntryInner is the implementation of DirEntry.
///
/// It specifically represents three distinct sources of directory entries:
///
/// 1. From the walkdir crate.
/// 2. Special entries that represent things like stdin.
/// 3. From a path.
///
/// Specifically, (3) has to essentially re-create the DirEntry implementation
/// from WalkDir.
#[derive(Debug)]
enum DirEntryInner {
    Stdin,
    Walkdir(walkdir::DirEntry),
    Raw(DirEntryRaw),
}

impl DirEntryInner {
    fn path(&self) -> &Path {
        use self::DirEntryInner::*;
        match *self {
            Stdin => Path::new("<stdin>"),
            Walkdir(ref x) => x.path(),
            Raw(ref x) => x.path(),
        }
    }

    fn path_is_symbolic_link(&self) -> bool {
        use self::DirEntryInner::*;
        match *self {
            Stdin => false,
            Walkdir(ref x) => x.path_is_symbolic_link(),
            Raw(ref x) => x.path_is_symbolic_link(),
        }
    }

    fn is_stdin(&self) -> bool {
        match *self {
            DirEntryInner::Stdin => true,
            _ => false,
        }
    }

    fn metadata(&self) -> Result<Metadata, Error> {
        use self::DirEntryInner::*;
        match *self {
            Stdin => {
                let err = Error::Io(io::Error::new(
                    io::ErrorKind::Other, "<stdin> has no metadata"));
                Err(err.with_path("<stdin>"))
            }
            Walkdir(ref x) => {
                x.metadata().map_err(|err| {
                    Error::Io(io::Error::from(err)).with_path(x.path())
                })
            }
            Raw(ref x) => x.metadata(),
        }
    }

    fn file_type(&self) -> Option<FileType> {
        use self::DirEntryInner::*;
        match *self {
            Stdin => None,
            Walkdir(ref x) => Some(x.file_type()),
            Raw(ref x) => Some(x.file_type()),
        }
    }

    fn file_name(&self) -> &OsStr {
        use self::DirEntryInner::*;
        match *self {
            Stdin => OsStr::new("<stdin>"),
            Walkdir(ref x) => x.file_name(),
            Raw(ref x) => x.file_name(),
        }
    }

    fn depth(&self) -> usize {
        use self::DirEntryInner::*;
        match *self {
            Stdin => 0,
            Walkdir(ref x) => x.depth(),
            Raw(ref x) => x.depth(),
        }
    }
}

/// DirEntryRaw is essentially copied from the walkdir crate so that we can
/// build `DirEntry`s from whole cloth in the parallel iterator.
#[derive(Debug)]
struct DirEntryRaw {
    /// The path as reported by the `fs::ReadDir` iterator (even if it's a
    /// symbolic link).
    path: PathBuf,
    /// The file type. Necessary for recursive iteration, so store it.
    ty: FileType,
    /// Is set when this entry was created from a symbolic link and the user
    /// expects the iterator to follow symbolic links.
    follow_link: bool,
    /// The depth at which this entry was generated relative to the root.
    depth: usize,
}

impl DirEntryRaw {
    fn path(&self) -> &Path {
        &self.path
    }

    fn path_is_symbolic_link(&self) -> bool {
        self.ty.is_symlink() || self.follow_link
    }

    fn metadata(&self) -> Result<Metadata, Error> {
        if self.follow_link {
            fs::metadata(&self.path)
        } else {
            fs::symlink_metadata(&self.path)
        }.map_err(|err| Error::Io(io::Error::from(err)).with_path(&self.path))
    }

    fn file_type(&self) -> FileType {
        self.ty
    }

    fn file_name(&self) -> &OsStr {
        self.path.file_name().unwrap_or_else(|| self.path.as_os_str())
    }

    fn depth(&self) -> usize {
        self.depth
    }

    fn from_entry(
        depth: usize,
        ent: &fs::DirEntry,
    ) -> Result<DirEntryRaw, Error> {
        let ty = try!(ent.file_type().map_err(|err| {
            let err = Error::Io(io::Error::from(err)).with_path(ent.path());
            Error::WithDepth {
                depth: depth,
                err: Box::new(err),
            }
        }));
        Ok(DirEntryRaw {
            path: ent.path(),
            ty: ty,
            follow_link: false,
            depth: depth,
        })
    }

    fn from_link(depth: usize, pb: PathBuf) -> Result<DirEntryRaw, Error> {
        let md = try!(fs::metadata(&pb).map_err(|err| {
            Error::Io(err).with_path(&pb)
        }));
        Ok(DirEntryRaw {
            path: pb,
            ty: md.file_type(),
            follow_link: true,
            depth: depth,
        })
    }

    fn from_path(depth: usize, pb: PathBuf) -> Result<DirEntryRaw, Error> {
        let md = try!(fs::symlink_metadata(&pb).map_err(|err| {
            Error::Io(err).with_path(&pb)
        }));
        Ok(DirEntryRaw {
            path: pb,
            ty: md.file_type(),
            follow_link: false,
            depth: depth,
        })
    }
}

/// WalkBuilder builds a recursive directory iterator.
///
/// The builder supports a large number of configurable options. This includes
/// specific glob overrides, file type matching, toggling whether hidden
/// files are ignored or not, and of course, support for respecting gitignore
/// files.
///
/// By default, all ignore files found are respected. This includes `.ignore`,
/// `.gitignore`, `.git/info/exclude` and even your global gitignore
/// globs, usually found in `$XDG_CONFIG_HOME/git/ignore`.
///
/// Some standard recursive directory options are also supported, such as
/// limiting the recursive depth or whether to follow symbolic links (disabled
/// by default).
///
/// # Ignore rules
///
/// There are many rules that influence whether a particular file or directory
/// is skipped by this iterator. Those rules are documented here. Note that
/// the rules assume a default configuration.
///
/// * First, glob overrides are checked. If a path matches a glob override,
/// then matching stops. The path is then only skipped if the glob that matched
/// the path is an ignore glob. (An override glob is a whitelist glob unless it
/// starts with a `!`, in which case it is an ignore glob.)
/// * Second, ignore files are checked. Ignore files currently only come from
/// git ignore files (`.gitignore`, `.git/info/exclude` and the configured
/// global gitignore file), plain `.ignore` files, which have the same format
/// as gitignore files, or explicitly added ignore files. The precedence order
/// is: `.ignore`, `.gitignore`, `.git/info/exclude`, global gitignore and
/// finally explicitly added ignore files. Note that precedence between
/// different types of ignore files is not impacted by the directory hierarchy;
/// any `.ignore` file overrides all `.gitignore` files. Within each
/// precedence level, more nested ignore files have a higher precedence over
/// less nested ignore files.
/// * Third, if the previous step yields an ignore match, than all matching
/// is stopped and the path is skipped.. If it yields a whitelist match, then
/// process continues. A whitelist match can be overridden by a later matcher.
/// * Fourth, unless the path is a directory, the file type matcher is run on
/// the path. As above, if it's an ignore match, then all matching is stopped
/// and the path is skipped. If it's a whitelist match, then matching
/// continues.
/// * Fifth, if the path hasn't been whitelisted and it is hidden, then the
/// path is skipped.
/// * Sixth, if the path has made it this far then it is yielded in the
/// iterator.
#[derive(Clone, Debug)]
pub struct WalkBuilder {
    paths: Vec<PathBuf>,
    ig_builder: IgnoreBuilder,
    parents: bool,
    max_depth: Option<usize>,
    follow_links: bool,
    threads: usize,
}

impl WalkBuilder {
    /// Create a new builder for a recursive directory iterator for the
    /// directory given.
    ///
    /// Note that if you want to traverse multiple different directories, it
    /// is better to call `add` on this builder than to create multiple
    /// `Walk` values.
    pub fn new<P: AsRef<Path>>(path: P) -> WalkBuilder {
        WalkBuilder {
            paths: vec![path.as_ref().to_path_buf()],
            ig_builder: IgnoreBuilder::new(),
            parents: true,
            max_depth: None,
            follow_links: false,
            threads: 0,
        }
    }

    /// Build a new `Walk` iterator.
    pub fn build(&self) -> Walk {
        let follow_links = self.follow_links;
        let max_depth = self.max_depth;
        let its = self.paths.iter().map(move |p| {
            if p == Path::new("-") {
                (p.to_path_buf(), None)
            } else {
                let mut wd = WalkDir::new(p);
                wd = wd.follow_links(follow_links || p.is_file());
                if let Some(max_depth) = max_depth {
                    wd = wd.max_depth(max_depth);
                }
                (p.to_path_buf(), Some(WalkEventIter::from(wd)))
            }
        }).collect::<Vec<_>>().into_iter();
        let ig_root = self.ig_builder.build();
        Walk {
            its: its,
            it: None,
            ig_root: ig_root.clone(),
            ig: ig_root.clone(),
            parents: self.parents,
        }
    }

    /// Build a new `WalkParallel` iterator.
    ///
    /// Note that this *doesn't* return something that implements `Iterator`.
    /// Instead, the returned value must be run with a closure. e.g.,
    /// `builder.build_parallel().run(|path| println!("{:?}", path))`.
    pub fn build_parallel(&self) -> WalkParallel {
        WalkParallel {
            paths: self.paths.clone().into_iter(),
            ig_root: self.ig_builder.build(),
            max_depth: self.max_depth,
            follow_links: self.follow_links,
            parents: self.parents,
            threads: self.threads,
        }
    }

    /// Add a file path to the iterator.
    ///
    /// Each additional file path added is traversed recursively. This should
    /// be preferred over building multiple `Walk` iterators since this
    /// enables reusing resources across iteration.
    pub fn add<P: AsRef<Path>>(&mut self, path: P) -> &mut WalkBuilder {
        self.paths.push(path.as_ref().to_path_buf());
        self
    }

    /// The maximum depth to recurse.
    ///
    /// The default, `None`, imposes no depth restriction.
    pub fn max_depth(&mut self, depth: Option<usize>) -> &mut WalkBuilder {
        self.max_depth = depth;
        self
    }

    /// Whether to follow symbolic links or not.
    pub fn follow_links(&mut self, yes: bool) -> &mut WalkBuilder {
        self.follow_links = yes;
        self
    }

    /// The number of threads to use for traversal.
    ///
    /// Note that this only has an effect when using `build_parallel`.
    ///
    /// The default setting is `0`, which chooses the number of threads
    /// automatically using heuristics.
    pub fn threads(&mut self, n: usize) -> &mut WalkBuilder {
        self.threads = n;
        self
    }

    /// Add an ignore file to the matcher.
    ///
    /// This has lower precedence than all other sources of ignore rules.
    ///
    /// If there was a problem adding the ignore file, then an error is
    /// returned. Note that the error may indicate *partial* failure. For
    /// example, if an ignore file contains an invalid glob, all other globs
    /// are still applied.
    pub fn add_ignore<P: AsRef<Path>>(&mut self, path: P) -> Option<Error> {
        let mut builder = GitignoreBuilder::new("");
        let mut errs = PartialErrorBuilder::default();
        errs.maybe_push_ignore_io(builder.add(path));
        match builder.build() {
            Ok(gi) => { self.ig_builder.add_ignore(gi); }
            Err(err) => { errs.push(err); }
        }
        errs.into_error_option()
    }

    /// Add an override matcher.
    ///
    /// By default, no override matcher is used.
    ///
    /// This overrides any previous setting.
    pub fn overrides(&mut self, overrides: Override) -> &mut WalkBuilder {
        self.ig_builder.overrides(overrides);
        self
    }

    /// Add a file type matcher.
    ///
    /// By default, no file type matcher is used.
    ///
    /// This overrides any previous setting.
    pub fn types(&mut self, types: Types) -> &mut WalkBuilder {
        self.ig_builder.types(types);
        self
    }

    /// Enables ignoring hidden files.
    ///
    /// This is enabled by default.
    pub fn hidden(&mut self, yes: bool) -> &mut WalkBuilder {
        self.ig_builder.hidden(yes);
        self
    }

    /// Enables reading ignore files from parent directories.
    ///
    /// If this is enabled, then the parent directories of each file path given
    /// are traversed for ignore files (subject to the ignore settings on
    /// this builder). Note that file paths are canonicalized with respect to
    /// the current working directory in order to determine parent directories.
    ///
    /// This is enabled by default.
    pub fn parents(&mut self, yes: bool) -> &mut WalkBuilder {
        self.parents = yes;
        self
    }

    /// Enables reading `.ignore` files.
    ///
    /// `.ignore` files have the same semantics as `gitignore` files and are
    /// supported by search tools such as ripgrep and The Silver Searcher.
    ///
    /// This is enabled by default.
    pub fn ignore(&mut self, yes: bool) -> &mut WalkBuilder {
        self.ig_builder.ignore(yes);
        self
    }

    /// Enables reading a global gitignore file, whose path is specified in
    /// git's `core.excludesFile` config option.
    ///
    /// Git's config file location is `$HOME/.gitconfig`. If `$HOME/.gitconfig`
    /// does not exist or does not specify `core.excludesFile`, then
    /// `$XDG_CONFIG_HOME/git/ignore` is read. If `$XDG_CONFIG_HOME` is not
    /// set or is empty, then `$HOME/.config/git/ignore` is used instead.
    pub fn git_global(&mut self, yes: bool) -> &mut WalkBuilder {
        self.ig_builder.git_global(yes);
        self
    }

    /// Enables reading `.gitignore` files.
    ///
    /// `.gitignore` files have match semantics as described in the `gitignore`
    /// man page.
    ///
    /// This is enabled by default.
    pub fn git_ignore(&mut self, yes: bool) -> &mut WalkBuilder {
        self.ig_builder.git_ignore(yes);
        self
    }

    /// Enables reading `.git/info/exclude` files.
    ///
    /// `.git/info/exclude` files have match semantics as described in the
    /// `gitignore` man page.
    ///
    /// This is enabled by default.
    pub fn git_exclude(&mut self, yes: bool) -> &mut WalkBuilder {
        self.ig_builder.git_exclude(yes);
        self
    }
}

/// Walk is a recursive directory iterator over file paths in one or more
/// directories.
///
/// Only file and directory paths matching the rules are returned. By default,
/// ignore files like `.gitignore` are respected. The precise matching rules
/// and precedence is explained in the documentation for `WalkBuilder`.
pub struct Walk {
    its: vec::IntoIter<(PathBuf, Option<WalkEventIter>)>,
    it: Option<WalkEventIter>,
    ig_root: Ignore,
    ig: Ignore,
    parents: bool,
}

impl Walk {
    /// Creates a new recursive directory iterator for the file path given.
    ///
    /// Note that this uses default settings, which include respecting
    /// `.gitignore` files. To configure the iterator, use `WalkBuilder`
    /// instead.
    pub fn new<P: AsRef<Path>>(path: P) -> Walk {
        WalkBuilder::new(path).build()
    }

    fn skip_entry(&self, ent: &walkdir::DirEntry) -> bool {
        if ent.depth() == 0 {
            return false;
        }
        skip_path(&self.ig, ent.path(), ent.file_type().is_dir())
    }
}

impl Iterator for Walk {
    type Item = Result<DirEntry, Error>;

    #[inline(always)]
    fn next(&mut self) -> Option<Result<DirEntry, Error>> {
        loop {
            let ev = match self.it.as_mut().and_then(|it| it.next()) {
                Some(ev) => ev,
                None => {
                    match self.its.next() {
                        None => return None,
                        Some((_, None)) => {
                            return Some(Ok(DirEntry::new_stdin()));
                        }
                        Some((path, Some(it))) => {
                            self.it = Some(it);
                            if self.parents && path.is_dir() {
                                let (ig, err) = self.ig_root.add_parents(path);
                                self.ig = ig;
                                if let Some(err) = err {
                                    return Some(Err(err));
                                }
                            } else {
                                self.ig = self.ig_root.clone();
                            }
                        }
                    }
                    continue;
                }
            };
            match ev {
                Err(err) => {
                    return Some(Err(Error::from(err)));
                }
                Ok(WalkEvent::Exit) => {
                    self.ig = self.ig.parent().unwrap();
                }
                Ok(WalkEvent::Dir(ent)) => {
                    if self.skip_entry(&ent) {
                        self.it.as_mut().unwrap().it.skip_current_dir();
                        // Still need to push this on the stack because
                        // we'll get a WalkEvent::Exit event for this dir.
                        // We don't care if it errors though.
                        let (igtmp, _) = self.ig.add_child(ent.path());
                        self.ig = igtmp;
                        continue;
                    }
                    let (igtmp, err) = self.ig.add_child(ent.path());
                    self.ig = igtmp;
                    return Some(Ok(DirEntry::new_walkdir(ent, err)));
                }
                Ok(WalkEvent::File(ent)) => {
                    if self.skip_entry(&ent) {
                        continue;
                    }
                    // If this isn't actually a file (e.g., a symlink),
                    // then skip it.
                    if !ent.file_type().is_file() {
                        continue;
                    }
                    return Some(Ok(DirEntry::new_walkdir(ent, None)));
                }
            }
        }
    }
}

/// WalkEventIter transforms a WalkDir iterator into an iterator that more
/// accurately describes the directory tree. Namely, it emits events that are
/// one of three types: directory, file or "exit." An "exit" event means that
/// the entire contents of a directory have been enumerated.
struct WalkEventIter {
    depth: usize,
    it: walkdir::Iter,
    next: Option<Result<walkdir::DirEntry, walkdir::Error>>,
}

#[derive(Debug)]
enum WalkEvent {
    Dir(walkdir::DirEntry),
    File(walkdir::DirEntry),
    Exit,
}

impl From<WalkDir> for WalkEventIter {
    fn from(it: WalkDir) -> WalkEventIter {
        WalkEventIter { depth: 0, it: it.into_iter(), next: None }
    }
}

impl Iterator for WalkEventIter {
    type Item = walkdir::Result<WalkEvent>;

    #[inline(always)]
    fn next(&mut self) -> Option<walkdir::Result<WalkEvent>> {
        let dent = self.next.take().or_else(|| self.it.next());
        let depth = match dent {
            None => 0,
            Some(Ok(ref dent)) => dent.depth(),
            Some(Err(ref err)) => err.depth(),
        };
        if depth < self.depth {
            self.depth -= 1;
            self.next = dent;
            return Some(Ok(WalkEvent::Exit));
        }
        self.depth = depth;
        match dent {
            None => None,
            Some(Err(err)) => Some(Err(err)),
            Some(Ok(dent)) => {
                if dent.file_type().is_dir() {
                    self.depth += 1;
                    Some(Ok(WalkEvent::Dir(dent)))
                } else {
                    Some(Ok(WalkEvent::File(dent)))
                }
            }
        }
    }
}

/// WalkParallel is a parallel recursive directory iterator over files paths
/// in one or more directories.
///
/// Only file and directory paths matching the rules are returned. By default,
/// ignore files like `.gitignore` are respected. The precise matching rules
/// and precedence is explained in the documentation for `WalkBuilder`.
///
/// Unlike `Walk`, this uses multiple threads for traversing a directory.
pub struct WalkParallel {
    paths: vec::IntoIter<PathBuf>,
    ig_root: Ignore,
    parents: bool,
    max_depth: Option<usize>,
    follow_links: bool,
    threads: usize,
}

impl WalkParallel {
    /// Execute the parallel recursive directory iterator. `f` is called for
    /// every file or directory.
    pub fn run<F>(
        self,
        f: F,
    ) where F: Fn(Result<DirEntry, Error>) + Send + Sync + 'static {
        let f = Arc::new(f);
        let queue = Arc::new(MsQueue::new());
        let num_waiting = Arc::new(AtomicUsize::new(0));
        let num_quitting = Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];
        for _ in 0..self.threads() {
            let worker = Worker {
                f: f.clone(),
                ig_root: self.ig_root.clone(),
                queue: queue.clone(),
                is_waiting: false,
                is_quitting: false,
                num_waiting: num_waiting.clone(),
                num_quitting: num_quitting.clone(),
                threads: self.threads(),
                parents: self.parents,
                max_depth: self.max_depth,
                follow_links: self.follow_links,
            };
            handles.push(thread::spawn(|| worker.run()));
        }
        for path in self.paths {
            if path == Path::new("-") {
                f(Ok(DirEntry::new_stdin()));
                continue;
            }
            let result = DirEntryRaw::from_path(0, path)
                .map(|raw| DirEntry::new_raw(raw, None));
            let dent = match result {
                Ok(dent) => dent,
                Err(err) => {
                    f(Err(err));
                    continue;
                }
            };
            if !dent.file_type().map_or(false, |t| t.is_dir()) {
                f(Ok(dent));
            } else {
                queue.push(Message::Work(Work {
                    dent: dent,
                    ignore: self.ig_root.clone(),
                }));
            }
        }
        for handle in handles {
            handle.join().unwrap();
        }
    }

    fn threads(&self) -> usize {
        if self.threads == 0 {
            2
        } else {
            self.threads
        }
    }
}

enum Message {
    Work(Work),
    Quit,
}

struct Work {
    dent: DirEntry,
    ignore: Ignore,
}

struct Worker {
    f: Arc<Fn(Result<DirEntry, Error>) + Send + Sync + 'static>,
    ig_root: Ignore,
    queue: Arc<MsQueue<Message>>,
    is_waiting: bool,
    is_quitting: bool,
    num_waiting: Arc<AtomicUsize>,
    num_quitting: Arc<AtomicUsize>,
    threads: usize,
    parents: bool,
    max_depth: Option<usize>,
    follow_links: bool,
}

impl Worker {
    fn run(mut self) {
        while let Some(mut work) = self.get_work() {
            let depth = work.dent.depth();
            if self.parents && depth == 0 {
                let (ig, err) = self.ig_root.add_parents(work.dent.path());
                work.ignore = ig;
                if let Some(err) = err {
                    (self.f)(Err(err));
                }
            }
            let readdir = match fs::read_dir(work.dent.path()) {
                Ok(readdir) => readdir,
                Err(err) => {
                    let err = Error::from(err)
                        .with_path(work.dent.path()).with_depth(depth);
                    (self.f)(Err(err));
                    continue;
                }
            };
            let (ig, err) = work.ignore.add_child(work.dent.path());
            work.ignore = ig;
            work.dent.err = err;
            (self.f)(Ok(work.dent));
            for result in readdir {
                let fs_dent = match result {
                    Ok(fs_dent) => fs_dent,
                    Err(err) => {
                        let err = Error::from(err).with_depth(depth + 1);
                        (self.f)(Err(err));
                        continue;
                    }
                };
                let result = DirEntryRaw::from_entry(depth + 1, &fs_dent)
                    .map(|raw| DirEntry::new_raw(raw, None));
                let dent = match result {
                    Ok(dent) => dent,
                    Err(err) => {
                        (self.f)(Err(err));
                        continue;
                    }
                };
                let is_dir = dent.file_type().map_or(false, |t| t.is_dir());
                let is_file = dent.file_type().map_or(false, |t| t.is_file());
                if skip_path(&work.ignore, dent.path(), is_dir) {
                    continue;
                }
                if !is_dir {
                    if is_file {
                        (self.f)(Ok(dent));
                    }
                } else {
                    self.queue.push(Message::Work(Work {
                        dent: dent,
                        ignore: work.ignore.clone(),
                    }));
                }
            }
        }
    }

    fn get_work(&mut self) -> Option<Work> {
        loop {
            match self.queue.try_pop() {
                Some(Message::Work(work)) => {
                    self.waiting(false);
                    self.quitting(false);
                    return Some(work);
                }
                Some(Message::Quit) => {
                    self.waiting(true);
                    self.quitting(true);
                    loop {
                        let nwait = self.num_waiting();
                        let nquit = self.num_quitting();
                        // If the number of waiting workers dropped, then
                        // abort our attempt to quit.
                        if nwait < self.threads {
                            break;
                        }
                        // If all workers are in this quit loop, then we
                        // can stop.
                        if nquit == self.threads {
                            return None;
                        }
                        // Otherwise, spin.
                    }
                    // If we're here, then we've aborted our quit attempt.
                    continue;
                }
                None => {
                    self.waiting(true);
                    self.quitting(false);
                    if self.num_waiting() == self.threads {
                        for _ in 0..self.threads {
                            self.queue.push(Message::Quit);
                        }
                    }
                    continue;
                }
            }
        }
    }

    fn num_waiting(&self) -> usize {
        self.num_waiting.load(Ordering::SeqCst)
    }

    fn num_quitting(&self) -> usize {
        self.num_quitting.load(Ordering::SeqCst)
    }

    fn quitting(&mut self, yes: bool) {
        if yes {
            if !self.is_quitting {
                self.is_quitting = true;
                self.num_quitting.fetch_add(1, Ordering::SeqCst);
            }
        } else {
            if self.is_quitting {
                self.is_quitting = false;
                self.num_quitting.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    fn waiting(&mut self, yes: bool) {
        if yes {
            if !self.is_waiting {
                self.is_waiting = true;
                self.num_waiting.fetch_add(1, Ordering::SeqCst);
            }
        } else {
            if self.is_waiting {
                self.is_waiting = false;
                self.num_waiting.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }
}

fn skip_path(ig: &Ignore, path: &Path, is_dir: bool) -> bool {
    let m = ig.matched(path, is_dir);
    if m.is_ignore() {
        debug!("ignoring {}: {:?}", path.display(), m);
        true
    } else if m.is_whitelist() {
        debug!("whitelisting {}: {:?}", path.display(), m);
        false
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::io::Write;
    use std::path::Path;

    use tempdir::TempDir;

    use super::{Walk, WalkBuilder};

    fn wfile<P: AsRef<Path>>(path: P, contents: &str) {
        let mut file = File::create(path).unwrap();
        file.write_all(contents.as_bytes()).unwrap();
    }

    fn mkdirp<P: AsRef<Path>>(path: P) {
        fs::create_dir_all(path).unwrap();
    }

    fn normal_path(unix: &str) -> String {
        if cfg!(windows) {
            unix.replace("\\", "/")
        } else {
            unix.to_string()
        }
    }

    fn walk_collect(prefix: &Path, walk: Walk) -> Vec<String> {
        let mut paths = vec![];
        for dent in walk {
            let dent = dent.unwrap();
            let path = dent.path().strip_prefix(prefix).unwrap();
            if path.as_os_str().is_empty() {
                continue;
            }
            paths.push(normal_path(path.to_str().unwrap()));
        }
        paths.sort();
        paths
    }

    fn mkpaths(paths: &[&str]) -> Vec<String> {
        let mut paths: Vec<_> = paths.iter().map(|s| s.to_string()).collect();
        paths.sort();
        paths
    }

    #[test]
    fn no_ignores() {
        let td = TempDir::new("walk-test-").unwrap();
        mkdirp(td.path().join("a/b/c"));
        mkdirp(td.path().join("x/y"));
        wfile(td.path().join("a/b/foo"), "");
        wfile(td.path().join("x/y/foo"), "");

        let got = walk_collect(td.path(), Walk::new(td.path()));
        assert_eq!(got, mkpaths(&[
            "x", "x/y", "x/y/foo", "a", "a/b", "a/b/foo", "a/b/c",
        ]));
    }

    #[test]
    fn gitignore() {
        let td = TempDir::new("walk-test-").unwrap();
        mkdirp(td.path().join("a"));
        wfile(td.path().join(".gitignore"), "foo");
        wfile(td.path().join("foo"), "");
        wfile(td.path().join("a/foo"), "");
        wfile(td.path().join("bar"), "");
        wfile(td.path().join("a/bar"), "");

        let got = walk_collect(td.path(), Walk::new(td.path()));
        assert_eq!(got, mkpaths(&["bar", "a", "a/bar"]));
    }

    #[test]
    fn explicit_ignore() {
        let td = TempDir::new("walk-test-").unwrap();
        let igpath = td.path().join(".not-an-ignore");
        mkdirp(td.path().join("a"));
        wfile(&igpath, "foo");
        wfile(td.path().join("foo"), "");
        wfile(td.path().join("a/foo"), "");
        wfile(td.path().join("bar"), "");
        wfile(td.path().join("a/bar"), "");

        let mut builder = WalkBuilder::new(td.path());
        assert!(builder.add_ignore(&igpath).is_none());
        let got = walk_collect(td.path(), builder.build());
        assert_eq!(got, mkpaths(&["bar", "a", "a/bar"]));
    }

    #[test]
    fn gitignore_parent() {
        let td = TempDir::new("walk-test-").unwrap();
        mkdirp(td.path().join("a"));
        wfile(td.path().join(".gitignore"), "foo");
        wfile(td.path().join("a/foo"), "");
        wfile(td.path().join("a/bar"), "");

        let root = td.path().join("a");
        let got = walk_collect(&root, Walk::new(&root));
        assert_eq!(got, mkpaths(&["bar"]));
    }
}
