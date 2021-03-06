use directories;
use failure::{bail, Fallible, ResultExt};
use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use serde_json;
use std::io::prelude::*;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use structopt::StructOpt;

mod cmdrunext;
mod podman;
use cmdrunext::CommandRunExt;

static DEFAULT_IMAGE: &str = "registry.fedoraproject.org/f30/fedora-toolbox:30";
/// The label set on toolbox images and containers.
static TOOLBOX_LABEL: &str = "com.coreos.toolbox";
/// The label set on github.com/debarshiray/fedora-toolbox images and containers.
static D_TOOLBOX_LABEL: &str = "com.github.debarshiray.toolbox";
/// The default container name
static DEFAULT_NAME: &str = "coreos-toolbox";
/// The path to our binary inside the container
static USR_BIN_SELF: &str = "/usr/bin/coretoolbox";
static STATE_ENV: &str = "TOOLBOX_STATE";

lazy_static! {
    static ref APPDIRS: directories::ProjectDirs =
        directories::ProjectDirs::from("com", "coreos", "toolbox").expect("creating appdirs");
}

static MAX_UID_COUNT: u32 = 65536;

/// Set of statically known paths to files/directories
/// that we redirect inside the container to /host.
static STATIC_HOST_FORWARDS: &[&str] = &["/run/dbus", "/run/libvirt", "/tmp", "/var/tmp"];
/// Set of devices we forward (if they exist)
static FORWARDED_DEVICES: &[&str] = &["bus", "dri", "kvm", "fuse"];

static PRESERVED_ENV: &[&str] = &[
    "COLORTERM",
    "DBUS_SESSION_BUS_ADDRESS",
    "DESKTOP_SESSION",
    "DISPLAY",
    "USER",
    "LANG",
    "SSH_AUTH_SOCK",
    "TERM",
    "VTE_VERSION",
    "XDG_CURRENT_DESKTOP",
    "XDG_DATA_DIRS",
    "XDG_MENU_PREFIX",
    "XDG_RUNTIME_DIR",
    "XDG_SEAT",
    "XDG_SESSION_DESKTOP",
    "XDG_SESSION_ID",
    "XDG_SESSION_TYPE",
    "XDG_VTNR",
    "WAYLAND_DISPLAY",
];

#[derive(Debug, StructOpt)]
struct CreateOpts {
    #[structopt(short = "I", long = "image")]
    /// Use a different base image
    image: Option<String>,

    #[structopt(short = "n", long = "name")]
    /// Name the container
    name: Option<String>,

    #[structopt(short = "N", long = "nested")]
    /// Allow running inside a container
    nested: bool,

    #[structopt(short = "D", long = "destroy")]
    /// Destroy any existing container
    destroy: bool,
}

#[derive(Debug, StructOpt)]
#[structopt(rename_all = "kebab-case")]
struct RunOpts {
    #[structopt(short = "n", long = "name")]
    /// Name of container
    name: Option<String>,

    #[structopt(short = "N", long = "nested")]
    /// Allow running inside a container
    nested: bool,

    #[structopt(long)]
    /// Run as (user namespace) root, do not change to unprivileged uid
    as_userns_root: bool,
}

#[derive(Debug, StructOpt)]
struct RmOpts {
    #[structopt(short = "n", long = "name", default_value = "coreos-toolbox")]
    /// Name for container
    name: String,
}

#[derive(Debug, StructOpt)]
#[structopt(name = "coretoolbox", about = "Toolbox")]
#[structopt(rename_all = "kebab-case")]
enum Opt {
    /// Create a toolbox
    Create(CreateOpts),
    /// Enter the toolbox
    Run(RunOpts),
    /// Delete the toolbox container
    Rm(RmOpts),
    /// Display names of already downloaded images with toolbox labels
    ListToolboxImages,
}

#[derive(Debug, StructOpt)]
#[structopt(rename_all = "kebab-case")]
struct ExecOpts {
    #[structopt(long)]
    /// See run --as-userns-root
    as_userns_root: bool,
}

#[derive(Debug, StructOpt)]
#[structopt(rename_all = "kebab-case")]
enum InternalOpt {
    /// Internal implementation detail; do not use
    RunPid1,
    /// Internal implementation detail; do not use
    Exec(ExecOpts),
}

fn get_toolbox_images() -> Fallible<Vec<podman::ImageInspect>> {
    let label = format!("label={}=true", TOOLBOX_LABEL);
    let mut ret = podman::image_inspect(&["--filter", label.as_str()]).with_context(|e| {
        format!(
            r#"Finding containers with label "{}": {}"#,
            TOOLBOX_LABEL, e
        )
    })?;
    let dlabel = format!("label={}=true", D_TOOLBOX_LABEL);
    ret.extend(
        podman::image_inspect(&["--filter", dlabel.as_str()]).with_context(|e| {
            format!(
                r#"Finding containers with label "{}": {}"#,
                D_TOOLBOX_LABEL, e
            )
        })?,
    );
    Ok(ret.drain(..).filter(|p| p.names.is_some()).collect())
}

/// Pull a container image if not present
fn ensure_image(name: &str) -> Fallible<()> {
    if !podman::has_object(podman::InspectType::Image, name)? {
        podman::cmd().args(&["pull", name]).run()?;
    }
    Ok(())
}

/// Parse an extant environment variable as UTF-8
fn getenv_required_utf8(n: &str) -> Fallible<String> {
    if let Some(v) = std::env::var_os(n) {
        Ok(v.to_str()
            .ok_or_else(|| failure::format_err!("{} is invalid UTF-8", n))?
            .to_string())
    } else {
        bail!("{} is unset", n)
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct EntrypointState {
    username: String,
    uid: u32,
    home: String,
}

fn append_preserved_env(c: &mut Command) -> Fallible<()> {
    for n in PRESERVED_ENV.iter() {
        let v = match std::env::var_os(n) {
            Some(v) => v,
            None => continue,
        };
        let v = v
            .to_str()
            .ok_or_else(|| failure::format_err!("{} contains invalid UTF-8", n))?;
        c.arg(format!("--env={}={}", n, v));
    }
    Ok(())
}

fn get_default_image() -> Fallible<String> {
    let toolboxes = get_toolbox_images()?;
    Ok(match toolboxes.len() {
        0 => {
            print!(
                "Welcome to coretoolbox
Enter a pull spec for toolbox image; default: {defimg}
Image: ",
                defimg = DEFAULT_IMAGE
            );
            std::io::stdout().flush()?;
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            input.truncate(input.trim_end().len());
            if input.is_empty() {
                DEFAULT_IMAGE.to_owned()
            } else {
                input
            }
        }
        1 => toolboxes[0].names.as_ref().unwrap()[0].clone(),
        _ => bail!("Multiple toolbox images found, must specify via -I"),
    })
}

/// Return the user's runtime directory, and create it if it doesn't exist.
/// The latter behavior is mostly necessary for running `sudo`.
fn get_ensure_runtime_dir() -> Fallible<String> {
    let real_uid: u32 = nix::unistd::getuid().into();
    let runtime_dir_val = std::env::var_os("XDG_RUNTIME_DIR");
    Ok(match runtime_dir_val.as_ref() {
        Some(d) => d
            .to_str()
            .ok_or_else(|| failure::format_err!("XDG_RUNTIME_DIR is invalid UTF-8"))?
            .to_string(),
        None => format!("/run/user/{}", real_uid),
    })
}

fn create(opts: &CreateOpts) -> Fallible<()> {
    if in_container() && !opts.nested {
        bail!("Already inside a container");
    }

    let image = if opts.image.is_none()
        && opts.name.is_none()
        && !podman::has_object(podman::InspectType::Container, DEFAULT_NAME)?
    {
        get_default_image()?
    } else {
        opts.image
            .as_ref()
            .map(|s| s.as_str())
            .unwrap_or(DEFAULT_IMAGE)
            .to_owned()
    };

    let name = opts
        .name
        .as_ref()
        .map(|s| s.as_str())
        .unwrap_or(DEFAULT_NAME);

    if opts.destroy {
        rm(&RmOpts {
            name: name.to_owned(),
        })?;
    }

    ensure_image(&image)?;

    // exec ourself as the entrypoint.  In the future this
    // would be better with podman fd passing.
    let self_bin = std::fs::read_link("/proc/self/exe")?;
    let self_bin = self_bin
        .as_path()
        .to_str()
        .ok_or_else(|| failure::err_msg("non-UTF8 self"))?;

    let real_uid: u32 = nix::unistd::getuid().into();
    let privileged = real_uid == 0;

    let runtime_dir = get_ensure_runtime_dir()?;
    std::fs::create_dir_all(&runtime_dir)?;

    let mut podman = podman::cmd();
    // The basic arguments.
    podman.args(&[
        "create",
        "--interactive",
        "--tty",
        "--hostname=toolbox",
        "--network=host",
        // We are not aiming for security isolation here; besides these, the
        // user's home directory is mounted in, so anything that wants to "escape"
        // can just mutate ~/.bashrc for example.
        "--ipc=host",
        "--privileged",
        "--security-opt=label=disable",
        "--tmpfs=/run:rw",
    ]);
    podman.arg(format!("--label={}=true", TOOLBOX_LABEL));
    podman.arg(format!("--name={}", name));
    // In privileged mode we assume we want to control all host processes by default;
    // we're more about debugging/management and less of a "dev container".
    if privileged {
        podman.arg("--pid=host");
    }
    // We bind ourself in so we can handle recursive invocation.
    podman.arg(format!("--volume={}:{}:ro", self_bin, USR_BIN_SELF));

    // In true privileged mode we don't use userns
    if !privileged {
        let uid_plus_one = real_uid + 1;
        let max_minus_uid = MAX_UID_COUNT - real_uid;
        podman.args(&[
            format!("--uidmap={}:0:1", real_uid),
            format!("--uidmap=0:1:{}", real_uid),
            format!(
                "--uidmap={}:{}:{}",
                uid_plus_one, uid_plus_one, max_minus_uid
            ),
        ]);
    }

    for p in &["/dev", "/usr", "/var", "/etc", "/run", "/tmp"] {
        podman.arg(format!("--volume={}:/host{}:rslave", p, p));
    }
    if Path::new("/sysroot").exists() {
        podman.arg("--volume=/sysroot:/host/sysroot:rslave");
    }
    if privileged {
        let debugfs = "/sys/kernel/debug";
        if Path::new(debugfs).exists() {
            // Bind debugfs in privileged mode so we can use e.g. bpftrace
            podman.arg(format!("--volume={}:{}:rslave", debugfs, debugfs));
        }
    }
    append_preserved_env(&mut podman)?;

    podman.arg(&image);
    podman.args(&[USR_BIN_SELF, "internals", "run-pid1"]);
    podman.stdout(Stdio::null());
    podman.run()?;
    Ok(())
}

fn in_container() -> bool {
    Path::new("/run/.containerenv").exists()
}

fn run(opts: &RunOpts) -> Fallible<()> {
    if in_container() && !opts.nested {
        bail!("Already inside a container");
    }

    let name = opts
        .name
        .as_ref()
        .map(|s| s.as_str())
        .unwrap_or(DEFAULT_NAME);

    if !podman::has_object(podman::InspectType::Container, &name)? {
        let toolboxes = get_toolbox_images()?;
        if toolboxes.len() == 0 {
            bail!("No toolbox container or images found; use `create` to create one")
        } else {
            bail!("No toolbox container '{}' found", name)
        }
    }

    podman::cmd()
        .args(&["start", name])
        .stdout(Stdio::null())
        .run()?;

    let mut podman = podman::cmd();
    podman.args(&["exec", "--interactive", "--tty"]);
    append_preserved_env(&mut podman)?;
    let state = EntrypointState {
        username: getenv_required_utf8("USER")?,
        uid: nix::unistd::getuid().into(),
        home: getenv_required_utf8("HOME")?,
    };
    let state = serde_json::to_string(&state)?;
    podman.arg(format!("--env={}={}", STATE_ENV, state.as_str()));
    podman.args(&[name, USR_BIN_SELF, "internals", "exec"]);
    if opts.as_userns_root {
        podman.arg("--as-userns-root");
    }
    return Err(podman.exec().into());
}

fn rm(opts: &RmOpts) -> Fallible<()> {
    if !podman::has_object(podman::InspectType::Container, opts.name.as_str())? {
        return Ok(());
    }
    let mut podman = podman::cmd();
    podman
        .args(&["rm", "-f", opts.name.as_str()])
        .stdout(Stdio::null());
    Err(podman.exec().into())
}

fn list_toolbox_images() -> Fallible<()> {
    let toolboxes = get_toolbox_images()?;
    if toolboxes.is_empty() {
        println!("No toolbox images found.")
    } else {
        for i in toolboxes {
            println!("{}", i.names.unwrap()[0]);
        }
    }
    Ok(())
}

mod entrypoint {
    use super::CommandRunExt;
    use super::{EntrypointState, ExecOpts};
    use failure::{bail, Fallible, ResultExt};
    use fs2::FileExt;
    use rayon::prelude::*;
    use std::fs::File;
    use std::io::prelude::*;
    use std::os::unix;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::Command;

    static CONTAINER_INITIALIZED_LOCK: &str = "/run/coreos-toolbox.lock";
    /// This file is created when we've generated a "container image" (overlayfs layer)
    /// that has things like our modifications to /etc/passwd, and the root `/`.
    static CONTAINER_INITIALIZED_STAMP: &str = "/etc/coreos-toolbox.initialized";
    /// This file is created when we've completed *runtime* state configuration
    /// changes such as bind mounts.
    static CONTAINER_INITIALIZED_RUNTIME_STAMP: &str = "/run/coreos-toolbox.initialized";

    /// Set of directories we explicitly make bind mounts rather than symlinks to /host.
    /// To ensure that paths are the same inside and out.
    static DATADIRS: &[&str] = &["/srv", "/mnt", "/home"];

    fn rbind<S: AsRef<Path>, D: AsRef<Path>>(src: S, dest: D) -> Fallible<()> {
        let src = src.as_ref();
        let dest = dest.as_ref();
        let mut c = Command::new("mount");
        c.arg("--rbind");
        c.arg(src);
        c.arg(dest);
        c.run()?;
        Ok(())
    }

    /// Update /etc/passwd with the same user from the host,
    /// and bind mount the homedir.
    fn adduser(state: &EntrypointState, with_sudo: bool) -> Fallible<()> {
        if state.uid == 0 {
            return Ok(());
        }
        let uidstr = format!("{}", state.uid);
        let mut cmd = Command::new("useradd");
        cmd.args(&[
            "--no-create-home",
            "--home-dir",
            &state.home,
            "--uid",
            &uidstr,
        ]);
        if with_sudo {
            cmd.args(&["--groups", "wheel"]);
        }
        cmd.arg(state.username.as_str());
        cmd.run()?;

        // Bind mount the homedir rather than use symlinks
        // as various software is unhappy if the path isn't canonical.
        std::fs::create_dir_all(&state.home)?;
        let uid = nix::unistd::Uid::from_raw(state.uid);
        let gid = nix::unistd::Gid::from_raw(state.uid);
        nix::unistd::chown(state.home.as_str(), Some(uid), Some(gid))?;
        let host_home = format!("/host{}", state.home);
        rbind(host_home.as_str(), state.home.as_str())?;
        Ok(())
    }

    /// Symlink a path e.g. /run/dbus/system_bus_socket to the
    /// /host equivalent, creating any necessary parent directories.
    fn host_symlink<P: AsRef<Path> + std::fmt::Display>(p: P) -> Fallible<()> {
        let path = p.as_ref();
        std::fs::create_dir_all(path.parent().unwrap())?;
        match std::fs::remove_dir_all(path) {
            Ok(_) => Ok(()),
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }?;
        unix::fs::symlink(format!("/host{}", p), path)?;
        Ok(())
    }

    fn init_container_static() -> Fallible<EntrypointState> {
        let initstamp = Path::new(CONTAINER_INITIALIZED_STAMP);

        let lockf = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(CONTAINER_INITIALIZED_LOCK)?;
        lockf.lock_exclusive()?;

        let state: EntrypointState =
            serde_json::from_str(super::getenv_required_utf8(super::STATE_ENV)?.as_str())?;

        if initstamp.exists() {
            return Ok(state);
        }

        let ostree_based_host = std::path::Path::new("/host/run/ostree-booted").exists();

        // Convert the container to ostree-style layout
        if ostree_based_host {
            DATADIRS.par_iter().try_for_each(|d| -> Fallible<()> {
                std::fs::remove_dir(d)?;
                let vard = format!("var{}", d);
                unix::fs::symlink(&vard, d)?;
                std::fs::create_dir(&vard)?;
                Ok(())
            })?;
        }

        // This is another mount point used by udisks
        unix::fs::symlink("/host/run/media", "/run/media")?;

        // Remove anaconda cruft
        std::fs::read_dir("/tmp")?.try_for_each(|e| -> Fallible<()> {
            let e = e?;
            if let Some(name) = e.file_name().to_str() {
                if name.starts_with("ks-script-") {
                    std::fs::remove_file(e.path())?;
                }
            }
            Ok(())
        })?;

        // These symlinks into /host are our set of default forwarded APIs/state
        // directories.
        super::STATIC_HOST_FORWARDS
            .par_iter()
            .try_for_each(host_symlink)
            .with_context(|e| format!("Enabling static host forwards: {}", e))?;

        let ostree_based_host = std::path::Path::new("/host/run/ostree-booted").exists();
        if ostree_based_host {
            unix::fs::symlink("sysroot/ostree", "/host/ostree")?;
        }

        // And these are into /dev
        if state.uid != 0 {
            super::FORWARDED_DEVICES
                .par_iter()
                .try_for_each(|d| -> Fallible<()> {
                    let devd = format!("/dev/{}", d);
                    let hostd = format!("/host{}", devd);
                    if !Path::new(&devd).exists() && Path::new(&hostd).exists() {
                        unix::fs::symlink(&hostd, &devd)
                            .with_context(|e| format!("symlinking {}: {}", d, e))?;
                    }
                    Ok(())
                })
                .with_context(|e| format!("Forwarding devices: {}", e))?;
        }

        // Allow sudo
        let mut with_sudo = false;
        if Path::new("/etc/sudoers.d").exists() {
            || -> Fallible<()> {
                let f = File::create(format!("/etc/sudoers.d/toolbox-{}", state.username))?;
                let mut perms = f.metadata()?.permissions();
                perms.set_readonly(true);
                f.set_permissions(perms)?;
                let mut f = std::io::BufWriter::new(f);
                writeln!(&mut f, "{} ALL=(ALL) NOPASSWD: ALL", state.username)?;
                f.flush()?;
                with_sudo = true;
                Ok(())
            }()
            .with_context(|e| format!("Enabling sudo: {}", e))?;
        }

        adduser(&state, with_sudo)?;
        let _ = File::create(&initstamp)?;

        Ok(state)
    }

    fn init_container_runtime() -> Fallible<()> {
        let initstamp = Path::new(CONTAINER_INITIALIZED_RUNTIME_STAMP);
        if initstamp.exists() {
            return Ok(());
        }

        let lockf = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(CONTAINER_INITIALIZED_LOCK)?;
        lockf.lock_exclusive()?;

        if initstamp.exists() {
            return Ok(());
        }

        // Forward the runtime dir
        {
            let runtime_dir = super::get_ensure_runtime_dir()?;
            let runtime_dir_p = std::path::Path::new(&runtime_dir);
            if !runtime_dir_p.exists() {
                std::fs::create_dir_all(runtime_dir_p.parent().expect("runtime dir parent"))?;
                host_symlink(runtime_dir)
                    .with_context(|e| format!("Forwarding runtime dir: {}", e))?;
            }
        }

        // Podman unprivileged mode has a bug where it exposes the host
        // selinuxfs which is bad because it can make e.g. librpm
        // think it can do domain transitions to rpm_exec_t, which
        // isn't actually permitted.
        let sysfs_selinux = "/sys/fs/selinux";
        if Path::new(sysfs_selinux).join("status").exists() {
            let empty_path = Path::new("/usr/share/empty");
            let empty_path = if empty_path.exists() {
                empty_path
            } else {
                let empty_path = Path::new("/usr/share/coretoolbox/empty");
                std::fs::create_dir_all(empty_path)?;
                empty_path
            };
            rbind(empty_path, sysfs_selinux)?;
        }

        let ostree_based_host = std::path::Path::new("/host/run/ostree-booted").exists();

        // Propagate standard mount points into the container.
        // We make these bind mounts instead of symlinks as
        // some programs get confused by absolute paths.
        if ostree_based_host {
            DATADIRS.par_iter().try_for_each(|d| -> Fallible<()> {
                let vard = format!("var{}", d);
                let hostd = format!("/host/{}", &vard);
                rbind(&hostd, &vard)?;
                Ok(())
            })?;
        } else {
            DATADIRS.par_iter().try_for_each(|d| -> Fallible<()> {
                let hostd = format!("/host/{}", d);
                rbind(&hostd, d)?;
                Ok(())
            })?;
        }

        Ok(())
    }

    pub(crate) fn exec(opts: ExecOpts) -> Fallible<()> {
        use nix::sys::stat::Mode;
        if !super::in_container() {
            bail!("Not inside a container");
        }
        let state = init_container_static()
            .with_context(|e| format!("Initializing container (static): {}", e))?;
        init_container_runtime()
            .with_context(|e| format!("Initializing container (runtime): {}", e))?;
        let initstamp = Path::new(CONTAINER_INITIALIZED_STAMP);
        if !initstamp.exists() {
            bail!("toolbox not initialized");
        }
        // Set a sane umask (022) by default; something seems to be setting it to 077
        nix::sys::stat::umask(Mode::S_IWGRP | Mode::S_IWOTH);
        let mut cmd = if opts.as_userns_root || !Path::new("/etc/sudoers.d").exists() {
            Command::new("/bin/bash")
        } else {
            let mut cmd = Command::new("setpriv");
            cmd.args(&[
                "--inh-caps=-all",
                "su",
                "--preserve-environment",
                state.username.as_str(),
            ])
            .env("HOME", state.home.as_str());
            cmd
        };

        Err(cmd.env_remove(super::STATE_ENV).exec().into())
    }

    pub(crate) fn run_pid1() -> Fallible<()> {
        unsafe {
            signal_hook::register(signal_hook::SIGCHLD, waitpid_all)?;
            signal_hook::register(signal_hook::SIGTERM, || std::process::exit(0))?;
        };
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1_000_000));
        }
    }

    fn waitpid_all() {
        use nix::sys::wait::WaitStatus;
        loop {
            match nix::sys::wait::waitpid(None, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
                Ok(status) => match status {
                    WaitStatus::StillAlive => break,
                    _ => {}
                },
                Err(_) => break,
            }
        }
    }
}

/// Primary entrypoint
fn main() {
    || -> Fallible<()> {
        let mut args: Vec<String> = std::env::args().collect();
        if let Some("internals") = args.get(1).map(|s| s.as_str()) {
            args.remove(1);
            let opts = InternalOpt::from_iter(args.iter());
            match opts {
                InternalOpt::Exec(execopts) => entrypoint::exec(execopts),
                InternalOpt::RunPid1 => entrypoint::run_pid1(),
            }
        } else {
            let opts = Opt::from_iter(args.iter());
            match opts {
                Opt::Create(ref opts) => create(opts),
                Opt::Run(ref opts) => run(opts),
                Opt::Rm(ref opts) => rm(opts),
                Opt::ListToolboxImages => list_toolbox_images(),
            }
        }
    }()
    .unwrap_or_else(|e| {
        eprintln!("error: {}", e);
        std::process::exit(1)
    })
}
