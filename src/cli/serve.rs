use std::{
    io::{self, Write},
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    sync::Arc,
};

use clap::Parser;
use memofs::Vfs;
use termcolor::{BufferWriter, Color, ColorChoice, ColorSpec, WriteColor};

use crate::{git::GitFilter, serve_session::ServeSession, web::LiveServer};

use super::{resolve_path, GlobalOptions};

const DEFAULT_BIND_ADDRESS: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);
const DEFAULT_PORT: u16 = 34872;

/// Expose a Rojo project to the Rojo Studio plugin.
#[derive(Debug, Parser)]
pub struct ServeCommand {
    /// Path to the project to serve. Defaults to the current directory.
    #[clap(default_value = "")]
    pub project: PathBuf,

    /// The IP address to listen on. Defaults to `127.0.0.1`.
    #[clap(long)]
    pub address: Option<IpAddr>,

    /// The port to listen on. Defaults to the project's preference, or `34872` if
    /// it has none.
    #[clap(long)]
    pub port: Option<u16>,

    /// Only sync files that have changed since the given Git reference.
    ///
    /// When this option is set, Rojo will only include files that have been
    /// modified, added, or are untracked since the specified Git reference
    /// (e.g., "HEAD", "main", a commit hash). This is useful for working with
    /// large projects where you only want to sync your local changes.
    ///
    /// Scripts that have not changed will still be acknowledged if modified
    /// during the session, and all synced instances will have
    /// ignoreUnknownInstances set to true to preserve descendants in Studio.
    #[clap(long, value_name = "REF")]
    pub git_since: Option<String>,
}

impl ServeCommand {
    pub fn run(self, global: GlobalOptions) -> anyhow::Result<()> {
        let project_path = resolve_path(&self.project);

        let vfs = Vfs::new_default();

        // Set up Git filter if --git-since was specified
        let git_filter = if let Some(ref base_ref) = self.git_since {
            let repo_root = GitFilter::find_repo_root(&project_path)?;
            log::info!(
                "Git filter enabled: only syncing files changed since '{}'",
                base_ref
            );
            Some(Arc::new(GitFilter::new(repo_root, base_ref.clone(), &project_path)?))
        } else {
            None
        };

        let session = Arc::new(ServeSession::new(vfs, project_path, git_filter)?);

        let ip = self
            .address
            .or_else(|| session.serve_address())
            .unwrap_or(DEFAULT_BIND_ADDRESS.into());

        let port = self
            .port
            .or_else(|| session.project_port())
            .unwrap_or(DEFAULT_PORT);

        let server = LiveServer::new(session);

        let _ = show_start_message(ip, port, self.git_since.as_deref(), global.color.into());
        server.start((ip, port).into());

        Ok(())
    }
}

fn show_start_message(
    bind_address: IpAddr,
    port: u16,
    git_since: Option<&str>,
    color: ColorChoice,
) -> io::Result<()> {
    let mut green = ColorSpec::new();
    green.set_fg(Some(Color::Green)).set_bold(true);

    let mut yellow = ColorSpec::new();
    yellow.set_fg(Some(Color::Yellow)).set_bold(true);

    let writer = BufferWriter::stdout(color);
    let mut buffer = writer.buffer();

    let address_string = if bind_address.is_loopback() {
        "localhost".to_owned()
    } else {
        bind_address.to_string()
    };

    writeln!(&mut buffer, "Rojo server listening:")?;

    write!(&mut buffer, "  Address: ")?;
    buffer.set_color(&green)?;
    writeln!(&mut buffer, "{}", address_string)?;

    buffer.set_color(&ColorSpec::new())?;
    write!(&mut buffer, "  Port:    ")?;
    buffer.set_color(&green)?;
    writeln!(&mut buffer, "{}", port)?;

    if let Some(base_ref) = git_since {
        buffer.set_color(&ColorSpec::new())?;
        write!(&mut buffer, "  Mode:    ")?;
        buffer.set_color(&yellow)?;
        writeln!(&mut buffer, "git-since ({})", base_ref)?;
    }

    writeln!(&mut buffer)?;

    buffer.set_color(&ColorSpec::new())?;
    write!(&mut buffer, "Visit ")?;

    buffer.set_color(&green)?;
    write!(&mut buffer, "http://{}:{}/", address_string, port)?;

    buffer.set_color(&ColorSpec::new())?;
    writeln!(&mut buffer, " in your browser for more information.")?;

    writer.print(&buffer)?;

    Ok(())
}
