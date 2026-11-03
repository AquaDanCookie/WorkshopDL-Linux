use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Locate the directory that holds steamcmd/, downloads/, config/ and sounds/.
pub fn project_dir() -> PathBuf {
    if let Ok(value) = env::var("WORKSHOPDL_PROJECT_DIR") {
        let path = PathBuf::from(value);
        if is_project_dir(&path) {
            return path;
        }
    }

    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if is_project_dir(&cwd) {
        return cwd;
    }
    let nested = cwd.join("Project");
    if is_project_dir(&nested) {
        return nested;
    }

    if let Ok(exe) = env::current_exe() {
        for ancestor in exe.ancestors().skip(1).take(5) {
            if is_project_dir(ancestor) {
                return ancestor.to_path_buf();
            }
            let nested = ancestor.join("Project");
            if is_project_dir(&nested) {
                return nested;
            }
        }
    }

    cwd
}

fn is_project_dir(path: &Path) -> bool {
    path.join("run.sh").is_file()
        || path.join("sounds").is_dir()
        || path.join("steamcmd").is_dir()
        || path.join("config").is_dir()
}

pub fn default_output_dir(project: &Path) -> PathBuf {
    project.join("downloads")
}

pub fn strip_ansi(text: &str) -> String {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").unwrap());
    re.replace_all(text, "").into_owned()
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Very small HTML entity decoder covering the entities Steam uses.
pub fn html_unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < text.len() {
        if bytes[i] != b'&' {
            let ch = text[i..].chars().next().unwrap_or('\u{fffd}');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let Some(end) = text[i..].find(';').map(|n| i + n) else {
            out.push('&');
            i += 1;
            continue;
        };
        if end - i > 32 {
            out.push('&');
            i += 1;
            continue;
        }
        let entity = &text[i + 1..end];
        let decoded = if let Some(hex) = entity
            .strip_prefix("#x")
            .or_else(|| entity.strip_prefix("#X"))
        {
            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
        } else if let Some(dec) = entity.strip_prefix('#') {
            dec.parse::<u32>().ok().and_then(char::from_u32)
        } else {
            match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                "ndash" => Some('–'),
                "mdash" => Some('—'),
                "hellip" => Some('…'),
                "lsquo" => Some('‘'),
                "rsquo" => Some('’'),
                "ldquo" => Some('“'),
                "rdquo" => Some('”'),
                "laquo" => Some('«'),
                "raquo" => Some('»'),
                "copy" => Some('©'),
                "reg" => Some('®'),
                "trade" => Some('™'),
                "times" => Some('×'),
                "middot" => Some('·'),
                "bull" => Some('•'),
                "deg" => Some('°'),
                "plusmn" => Some('±'),
                "frac12" => Some('½'),
                "frac14" => Some('¼'),
                "frac34" => Some('¾'),
                "euro" => Some('€'),
                "pound" => Some('£'),
                "yen" => Some('¥'),
                "cent" => Some('¢'),
                "sect" => Some('§'),
                "para" => Some('¶'),
                "dagger" => Some('†'),
                "Dagger" => Some('‡'),
                "permil" => Some('‰'),
                "prime" => Some('′'),
                "Prime" => Some('″'),
                "larr" => Some('←'),
                "rarr" => Some('→'),
                "uarr" => Some('↑'),
                "darr" => Some('↓'),
                "harr" => Some('↔'),
                "hearts" => Some('♥'),
                "spades" => Some('♠'),
                "clubs" => Some('♣'),
                "diams" => Some('♦'),
                "star" => Some('☆'),
                "starf" => Some('★'),
                "check" => Some('✓'),
                "cross" => Some('✗'),
                "phone" => Some('☎'),
                "female" => Some('♀'),
                "male" => Some('♂'),
                _ => None,
            }
        };
        if let Some(ch) = decoded {
            out.push(ch);
            i = end + 1;
        } else {
            out.push('&');
            i += 1;
        }
    }
    out
}

pub fn open_path(path: &Path) {
    let _ = Command::new("xdg-open")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

pub fn open_url(url: &str) {
    let _ = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Play sounds/finish.*. The system almost always has one of these players.
pub fn play_finish_sound(project: &Path) {
    let sounds = project.join("sounds");
    let candidates = [
        "finish.mp3",
        "finish.ogg",
        "finish.flac",
        "finish.wav",
        "finish.m4a",
    ];
    let Some(path) = candidates
        .iter()
        .map(|name| sounds.join(name))
        .find(|path| path.is_file())
    else {
        play_beep();
        return;
    };

    let attempts: &[(&str, &[&str])] = &[
        ("ffplay", &["-nodisp", "-autoexit", "-loglevel", "quiet"]),
        ("mpv", &["--really-quiet", "--no-video"]),
        ("paplay", &[]),
        ("pw-play", &[]),
        ("aplay", &["-q"]),
    ];
    for (program, args) in attempts {
        if Command::new(program)
            .args(*args)
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
        {
            return;
        }
    }
    play_beep();
}

fn play_beep() {
    let _ = fs::write("/dev/tty", b"\x07");
}

pub fn recursive_copy(source: &Path, dest: &Path) -> std::io::Result<()> {
    if source.is_dir() {
        fs::create_dir_all(dest)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let target = dest.join(entry.file_name());
            if file_type.is_dir() {
                recursive_copy(&entry.path(), &target)?;
            } else if file_type.is_symlink() {
                let link = fs::read_link(entry.path())?;
                #[cfg(unix)]
                std::os::unix::fs::symlink(link, &target)?;
            } else {
                fs::copy(entry.path(), &target)?;
            }
        }
    } else {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, dest)?;
    }
    Ok(())
}

pub fn move_path(source: &Path, dest: &Path) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(source, dest) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::EXDEV) => {
            recursive_copy(source, dest)?;
            if source.is_dir() {
                fs::remove_dir_all(source)?;
            } else {
                fs::remove_file(source)?;
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

pub fn dir_has_files(path: &Path) -> bool {
    fs::read_dir(path)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}
