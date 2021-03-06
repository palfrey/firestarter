use std::ffi::OsStr;
use std::fs::{remove_file, rename, File, OpenOptions};
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local, Timelike, Utc};
use failure::{err_msg, Error};
use glob::glob;

const MIDNIGHT: u64 = 60 * 60 * 24;

pub trait RotatePolicy {
    fn rotate(&mut self, buf: &[u8], p: &Path, file: &File) -> io::Result<bool>;
}

struct SizeRotatePolicy {
    max_file_size: u64,
    max_backup: u32,
}

impl SizeRotatePolicy {
    fn new(max_file_size: u64, max_backup: u32) -> Self {
        SizeRotatePolicy {
            max_file_size,
            max_backup,
        }
    }
}

impl RotatePolicy for SizeRotatePolicy {
    fn rotate(&mut self, buf: &[u8], p: &Path, file: &File) -> io::Result<bool> {
        let max_file_size = self.max_file_size;
        let max_backup = self.max_backup;
        let buf_len = buf.len() as u64;

        let metadata = file.metadata()?;
        let size = metadata.len();
        if max_file_size > buf_len + size {
            return Ok(false);
        }
        if !p.exists() {
            return Ok(false);
        }
        let (parent, name, ext) = get_log_names(p);

        let file_name = if let Some(log_ext) = Path::new(name).extension().and_then(OsStr::to_str) {
            let mut log_num: u32 = ext.parse().unwrap();
            log_num += 1;
            if log_num > max_backup {
                return Ok(false);
            }
            let name = Path::new(name).file_stem().and_then(OsStr::to_str).unwrap();
            format!("{}.{}.{}", name, log_ext, log_num)
        } else {
            let name = Path::new(name).to_str().unwrap();
            format!("{}.{}.1", name, ext)
        };

        let pbuf = Path::new(parent).join(file_name);
        let new_path = pbuf.as_path();
        if new_path.exists() {
            self.rotate(buf, new_path, file)?;
        }
        debug!("rename backup log file. {:?} -> {:?}", p, new_path);
        rename(p, new_path)?;
        Ok(true)
    }
}

struct TimedRotatePolicy {
    rollover_at: SystemTime,
    duration: Duration,
    format: String,
    max_backup: u32,
    check_time: SystemTime,
    midnight: bool,
    utc: bool,
}

impl TimedRotatePolicy {
    fn new(interval: u32, when: &str, utc: &str, max_backup: u32, log_file: &str) -> Self {
        let utc = utc == "U" || utc == "UTC";
        let (interval_secs, fmt, midnight): (u64, &str, bool) = match when {
            "S" => (1, "%Y%m%d%H%M%S", false),
            "M" => (60, "%Y%m%d%H%M", false),
            "H" => (60 * 60, "%Y%m%d%H", false),
            "D" => (60 * 60 * 24, "%Y%m%d", false),
            "MIDNIGHT" => (60 * 60 * 24, "%Y%m%d", true),
            _ => panic!("rotate unknown type"),
        };
        let duration = if when == "MIDNIGHT" {
            Duration::from_secs(interval_secs)
        } else {
            Duration::from_secs(interval_secs * u64::from(interval))
        };
        let f = Path::new(log_file);
        let now = if f.exists() {
            let mdata = f.metadata().unwrap();
            mdata.modified().unwrap()
        } else {
            SystemTime::now()
        };
        let rollover_at = TimedRotatePolicy::compute_rollover(now, utc, midnight, duration);
        let check_time = SystemTime::now();

        TimedRotatePolicy {
            rollover_at,
            duration,
            format: fmt.to_owned(),
            max_backup,
            check_time,
            midnight,
            utc,
        }
    }

    fn compute_rollover(
        now: SystemTime,
        utc: bool,
        midnight: bool,
        duration: Duration,
    ) -> SystemTime {
        if midnight {
            let (current_hour, current_minute, current_second) = if utc {
                let now: DateTime<Utc> = DateTime::from(now);
                (now.hour(), now.minute(), now.second())
            } else {
                let now: DateTime<Local> = DateTime::from(now);
                (now.hour(), now.minute(), now.second())
            };

            let delta =
                MIDNIGHT - u64::from((current_hour * 60 + current_minute) * 60 + current_second);
            debug!("MIDNIGHT delta {} real duration {:?} ", delta, duration);
            now + Duration::from_secs(delta)
        } else {
            now + duration
        }
    }

    fn get_timed_filename(&mut self, p: &Path) -> PathBuf {
        let (parent, name, ext) = get_log_names(p);
        let delta = self.rollover_at - self.duration;
        let suffix = if self.utc {
            let t: DateTime<Utc> = DateTime::from(delta);
            t.format(&self.format)
        } else {
            let t: DateTime<Local> = DateTime::from(delta);
            t.format(&self.format)
        };
        let file_name = format!("{}.{}.{}", name, ext, suffix);
        Path::new(parent).join(file_name)
    }

    fn timed_rotate(&mut self, now: SystemTime, p: &Path) -> io::Result<bool> {
        let max_backup = self.max_backup;
        if self.rollover_at > now {
            return Ok(false);
        }
        if !p.exists() {
            return Ok(false);
        }
        let new_path = self.get_timed_filename(p);
        if new_path.exists() {
            remove_file(&new_path)?;
        }
        debug!("rename backup log file. {:?} -> {:?}", p, new_path);
        rename(p, &new_path)?;
        TimedRotatePolicy::remove_old_backup(p, max_backup as usize)?;
        let mut new_rollover_at =
            TimedRotatePolicy::compute_rollover(now, self.utc, self.midnight, self.duration);
        while new_rollover_at <= now {
            new_rollover_at += self.duration;
        }
        self.rollover_at = new_rollover_at;
        Ok(true)
    }

    fn remove_old_backup(p: &Path, max_backup: usize) -> io::Result<()> {
        if let Ok(paths) = glob(&format!("{}.*", p.to_str().unwrap())) {
            let mut tmp = Vec::new();
            for entry in paths {
                match entry {
                    Ok(path) => {
                        tmp.push(path);
                    }
                    Err(e) => {
                        warn!("fail get path. caused by: {}", e);
                    }
                }
            }
            let size = tmp.len();
            if size > max_backup {
                tmp.sort();
                for p in tmp.drain(0..size - max_backup) {
                    remove_file(&p)?;
                    debug!("remove backup {:?}", &p);
                }
            }
        }
        Ok(())
    }
}

impl RotatePolicy for TimedRotatePolicy {
    fn rotate(&mut self, _buf: &[u8], p: &Path, _file: &File) -> io::Result<bool> {
        if let Ok(elapsed) = self.check_time.elapsed() {
            if elapsed.as_secs() >= 1 {
                self.check_time = SystemTime::now();
                return self.timed_rotate(SystemTime::now(), p);
            }
        }
        Ok(false)
    }
}

pub struct LogFile {
    inner: Option<File>,
    log_file: PathBuf,
    policy: Box<RotatePolicy>,
}

impl LogFile {
    pub fn new(log_file: PathBuf, policy: Box<RotatePolicy>) -> Self {
        LogFile {
            inner: None,
            log_file,
            policy,
        }
    }

    pub fn open(&mut self) -> io::Result<()> {
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.log_file.as_path())?;
        self.inner = Some(file);
        Ok(())
    }

    fn try_rotate(&mut self, buf: &[u8]) -> io::Result<()> {
        let newfile = if let Some(ref mut inner) = self.inner {
            inner.flush()?;

            let &mut LogFile {
                ref log_file,
                ref mut policy,
                ..
            } = self;

            if policy.rotate(buf, log_file, &inner)? {
                let file = OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(log_file)?;
                Some(file)
            } else {
                None
            }
        } else {
            None
        };

        if newfile.is_some() {
            self.inner = newfile;
        }

        Ok(())
    }
}

impl Write for LogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.try_rotate(buf)?;
        if let Some(ref mut inner) = self.inner {
            inner.write(buf)
        } else {
            Ok(0)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(ref mut inner) = self.inner {
            inner.flush()
        } else {
            Ok(())
        }
    }
}

impl FromStr for LogFile {
    type Err = Error;

    fn from_str(s: &str) -> Result<LogFile, Error> {
        let log_cfg: Vec<&str> = s.split(':').collect();
        let log_type = log_cfg[0];
        match log_type {
            "size" => {
                // size:100000:5:/tmp.log
                let max_file_size: u64 = log_cfg[1].parse().unwrap();
                let max_backup: u32 = log_cfg[2].parse().unwrap();
                let path = log_cfg[3];
                let policy = SizeRotatePolicy::new(max_file_size, max_backup);
                let log = LogFile::new(PathBuf::from(path), Box::new(policy));
                Ok(log)
            }
            "time" => {
                // time:7:D:U:5:/tmp.log
                let roll_over: u32 = log_cfg[1].parse().unwrap();
                let when = log_cfg[2];
                let utc = log_cfg[3];
                let max_backup: u32 = log_cfg[4].parse().unwrap();
                let path = log_cfg[5];

                let utc = utc.to_uppercase();
                let when = when.to_uppercase();
                let policy = TimedRotatePolicy::new(roll_over, &when, &utc, max_backup, path);
                let log = LogFile::new(PathBuf::from(path), Box::new(policy));
                Ok(log)
            }
            _ => Err(err_msg("unknown log type")),
        }
    }
}

fn get_log_names(p: &Path) -> (&Path, &str, &str) {
    let log_ext = p.extension().unwrap_or_else(|| OsStr::new(""));
    let parent = p.parent().unwrap();
    let name = match p.file_stem() {
        Some(f) => f,
        None => p.file_name().unwrap(),
    };
    let name = name.to_str().unwrap();
    let ext = log_ext.to_str().unwrap();
    (parent, name, ext)
}
