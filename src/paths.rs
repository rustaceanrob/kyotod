use std::path::PathBuf;

pub fn expand(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~/") {
        let home = std::env::var("HOME").expect("HOME is not set");
        PathBuf::from(home).join(rest)
    } else if input == "~" {
        PathBuf::from(std::env::var("HOME").expect("HOME is not set"))
    } else {
        PathBuf::from(input)
    }
}

pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn new(datadir: &str) -> std::io::Result<Self> {
        let root = expand(datadir);
        std::fs::create_dir_all(root.join("wallets"))?;
        std::fs::create_dir_all(root.join("data"))?;
        Ok(Self { root })
    }

    pub fn wallets_dir(&self) -> PathBuf {
        self.root.join("wallets")
    }

    pub fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    pub fn socket(&self) -> PathBuf {
        self.root.join("node.sock")
    }
}
