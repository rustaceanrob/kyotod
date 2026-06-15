use std::{
    fs::{self, File},
    os::fd::IntoRawFd,
};

const MASK_OCTAL: libc::mode_t = 0o027;
const DEV_NULL: &str = "/dev/null";

pub struct Daemonize {
    working_directory: String,
}

impl Daemonize {
    pub fn new(working_directory: String) -> Self {
        Self { working_directory }
    }

    pub fn fork(self) -> std::io::Result<()> {
        match unsafe { libc::fork() } {
            -1 => return Err(std::io::Error::last_os_error()),
            0 => (),
            _pid => std::process::exit(0),
        }
        if unsafe { libc::setsid() } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        std::env::set_current_dir(&self.working_directory)?;
        unsafe { libc::umask(MASK_OCTAL) };
        let null_std_in = File::open(DEV_NULL)?;
        unsafe { libc::dup2(null_std_in.into_raw_fd(), libc::STDIN_FILENO) };
        let pid_file = format!("{}/node.pid", self.working_directory);
        let process_id = std::process::id();
        fs::write(pid_file, process_id.to_string())?;
        let log_file = format!("{}/node.log", self.working_directory);
        let log_file = File::create(log_file)?;
        unsafe { libc::dup2(log_file.into_raw_fd(), libc::STDOUT_FILENO) };
        unsafe { libc::dup2(libc::STDOUT_FILENO, libc::STDERR_FILENO) };
        Ok(())
    }
}
