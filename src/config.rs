use crate::{Result, cli, fail};
use std::{env, fs, io, path::PathBuf};

#[derive(Default)]
pub struct Config {
    pub session_dir: Option<PathBuf>,
    pub options: cli::Options,
}
impl Config {
    pub fn parse(text: &str) -> Result<Self> {
        let mut config = Self::default();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let parsed = (|| -> Result<()> {
                let (key, value) = line.split_once('=').ok_or("expected key = value")?;
                let value = value.trim();
                let o = &mut config.options;
                match key.trim() {
                    "session_dir" => config.session_dir = Some(value.into()),
                    "quiet" => o.quiet = value.parse()?,
                    "detach_key" => {
                        o.detach = if value == "none" {
                            None
                        } else {
                            Some(cli::detach_key(value)?)
                        }
                    }
                    "suspend" => o.suspend = value.parse()?,
                    "ansi" => o.ansi = value.parse()?,
                    "log_size" => o.cap = cli::size(value)?,
                    "redraw" if ["none", "winch", "ctrl_l"].contains(&value) => {
                        o.redraw = value.into();
                    }
                    "clear_mode" if ["none", "move"].contains(&value) => o.clear = value.into(),
                    "tail_lines" => o.lines = value.parse()?,
                    "tail_follow" => o.follow = value.parse()?,
                    key => {
                        return fail(format!("unknown setting or invalid value: {key} = {value}"));
                    }
                }
                Ok(())
            })();
            parsed.map_err(|e| format!("line {}: {e}", i + 1))?;
        }
        Ok(config)
    }
}
pub fn load() -> Result<Config> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is not set")?;
    let path = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
        .join("rtch/config");
    match fs::read_to_string(&path) {
        Ok(text) => Config::parse(&text).map_err(|e| format!("{}: {e}", path.display()).into()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_settings_have_line_numbers() {
        for line in [
            "quiet = yes",
            "log_size = 257m",
            "detach_key = abc",
            "redraw = bad",
            "clear_mode = bad",
            "tail_lines = -1",
            "force = true",
            "all = true",
            "oops",
        ] {
            let error = Config::parse(&format!("# comment\n{line}"))
                .err()
                .unwrap()
                .to_string();
            assert!(error.contains("line 2:"), "{error}");
        }
    }
}
