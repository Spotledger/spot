//! Spotledger command-line entry point.
use clap::{CommandFactory, FromArgMatches, Subcommand};
use std::path::PathBuf;

/// Registered commands: one file each in `src/commands/` (see `commands/mod.rs`).
mod commands;

#[derive(clap::Parser)]
#[command(
    name = "spot",
    about = "Spotledger site management and native API host"
)]
struct Args {
    #[arg(long, global = true, env = "SPOT_ROOT", default_value = ".")]
    root: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Create a site and its database; prints the Administrator's first sign-in secret once.
    NewSite {
        site: String,
        #[arg(long, default_value = "en")]
        collation: String,
        /// Install only the platform app; add apps later with `install-app`.
        #[arg(long)]
        bare: bool,
        /// Hours the first sign-in secret works (1 to 336; default 72).
        #[arg(long, value_name = "HOURS")]
        secret_hours: Option<u32>,
        /// Also write a separate Administrator API key to this new file (mode 0600), for a
        /// script. It expires, and can only change the password until `set-password` runs.
        #[arg(long, value_name = "PATH")]
        admin_api_key_file: Option<PathBuf>,
        /// Hours the API key in --admin-api-key-file works (1 to 168; default 24).
        #[arg(long, value_name = "HOURS", requires = "admin_api_key_file")]
        admin_api_key_hours: Option<u32>,
    },
    /// Run the native API host.
    Serve {
        #[arg(long)]
        dev: bool,
    },
    /// Validate an app and install it on a site (dry run unless --apply).
    InstallApp {
        /// An app name from the catalogue in common.toml, or its directory.
        app: String,
        #[arg(long)]
        site: Option<String>,
        /// Change the schema and record the install. Without it only the plan is shown.
        #[arg(long)]
        apply: bool,
        /// Load the app's seed.toml after installing.
        #[arg(long)]
        seed: bool,
    },
    /// List the apps installed on a site (or every site) and the apps available.
    ListApps {
        #[arg(long)]
        site: Option<String>,
    },
    /// Plan, and with --apply run, the schema change for the installed apps' metadata.
    Migrate {
        #[arg(long)]
        site: Option<String>,
        /// Only show the plan (the default).
        #[arg(long, conflicts_with = "apply")]
        dry_run: bool,
        /// Apply the plan.
        #[arg(long)]
        apply: bool,
        /// Refuse to apply unless the plan has exactly this hash.
        #[arg(long, requires = "apply")]
        plan_hash: Option<String>,
    },
    /// Create an invited user; prints a one-time first sign-in secret once.
    AddUser {
        site: String,
        email: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long = "role")]
        roles: Vec<String>,
        /// Hours the first sign-in secret works (1 to 336; default 72).
        #[arg(long, value_name = "HOURS")]
        secret_hours: Option<u32>,
    },
    /// Set a user's password, read from the terminal (echo off) or standard input.
    SetPassword {
        site: String,
        user: String,
        /// Read the password from one line of standard input.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Replace the Administrator's credentials with a new one-time secret.
    ResetAdmin {
        site: String,
        /// Confirms that the person running this command is the site operator.
        #[arg(long)]
        yes_i_am_the_operator: bool,
        /// Hours the new one-time secret works (1 to 336; default 72).
        #[arg(long, value_name = "HOURS")]
        secret_hours: Option<u32>,
    },
    /// Ask a running host to rebuild a site's metadata and permission snapshot.
    Reload {
        #[arg(long)]
        site: Option<String>,
    },
    /// Validate configuration, apps and database connections without starting the host.
    Check,
}
#[tokio::main]
async fn main() -> std::process::ExitCode {
    // The derive command is the base; the registered commands are added to it, so `--help` lists
    // both and an unknown name lists every command.
    let command = match commands::augment(Args::command(), commands::COMMANDS) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("{message}");
            return commands::Exit::Failed.into();
        }
    };
    let matches = match command.clone().try_get_matches() {
        Ok(matches) => matches,
        Err(error) => {
            if let Some(message) = commands::unknown_command_message(&command, &error) {
                eprint!("{message}");
                return commands::Exit::Usage.into();
            }
            error.exit()
        }
    };
    if commands::selected(commands::COMMANDS, &matches).is_some() {
        let root = matches
            .get_one::<PathBuf>("root")
            .cloned()
            .unwrap_or_else(|| PathBuf::from("."));
        let mut ctx = commands::CommandCtx::new(
            root,
            std::sync::Arc::new(spot_core::SystemClock::new()),
            Box::new(std::io::stdout()),
            Box::new(std::io::stderr()),
        );
        return match commands::run_registered(commands::COMMANDS, &matches, &mut ctx).await {
            Some(exit) => exit.into(),
            None => commands::Exit::Usage.into(),
        };
    }
    let args = match Args::from_arg_matches(&matches) {
        Ok(args) => args,
        Err(error) => error.exit(),
    };
    match run(args).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        }
    }
}
async fn run(args: Args) -> spot_cli::Result<()> {
    match args.command {
        Command::NewSite {
            site,
            collation,
            bare,
            secret_hours,
            admin_api_key_file,
            admin_api_key_hours,
        } => {
            let mut issue = issue_now();
            if let Some(hours) = secret_hours {
                issue = issue.secret_hours(hours)?;
            }
            if let Some(hours) = admin_api_key_hours {
                issue = issue.key_hours(hours)?;
            }
            let kind = if bare {
                spot_cli::SiteKind::Bare
            } else {
                spot_cli::SiteKind::HostWide
            };
            let secret = spot_cli::new_site_with(
                &args.root,
                &site,
                &collation,
                kind,
                &issue,
                admin_api_key_file.as_deref(),
            )
            .await?;
            println!(
                "Administrator first sign-in secret (shown once, {}): {}",
                secret_note(&issue),
                secret.text()
            );
            if let Some(path) = &admin_api_key_file {
                eprintln!(
                    "Administrator API key written to {} (mode 0600, expires in {} hours, limited \
                     to changing the password until `spot set-password` runs).",
                    path.display(),
                    (issue.key_expires_at() - issue.issued_at()) / 3600
                );
            }
        }
        Command::InstallApp {
            app,
            site,
            apply,
            seed,
        } => {
            let options = spot_cli::ops::Install { apply, seed };
            print!(
                "{}",
                spot_cli::ops::install_app(&args.root, &app, site.as_deref(), options).await?
            );
        }
        Command::ListApps { site } => {
            print!("{}", spot_cli::ops::list_apps(&args.root, site.as_deref())?);
        }
        Command::Migrate {
            site,
            dry_run: _,
            apply,
            plan_hash,
        } => {
            let options = spot_cli::ops::Migrate { apply, plan_hash };
            print!(
                "{}",
                spot_cli::ops::migrate(&args.root, site.as_deref(), options).await?
            );
        }
        Command::AddUser {
            site,
            email,
            name,
            roles,
            secret_hours,
        } => {
            let mut issue = issue_now();
            if let Some(hours) = secret_hours {
                issue = issue.secret_hours(hours)?;
            }
            let invitation = spot_cli::users::add_user_with(
                &args.root,
                &site,
                &email,
                name.as_deref(),
                &roles,
                &issue,
            )
            .await?;
            println!(
                "User {} created (invited). First sign-in secret (shown once, {}): {}",
                invitation.user,
                secret_note(&issue),
                invitation.secret.text()
            );
        }
        Command::SetPassword {
            site,
            user,
            password_stdin,
        } => {
            let password = spot_cli::users::read_password(password_stdin)?;
            spot_cli::users::set_password(&args.root, &site, &user, &password).await?;
            println!("Password set for {user}.");
        }
        Command::ResetAdmin {
            site,
            yes_i_am_the_operator,
            secret_hours,
        } => {
            let mut issue = issue_now();
            if let Some(hours) = secret_hours {
                issue = issue.secret_hours(hours)?;
            }
            let secret = spot_cli::users::reset_admin_checked(
                &args.root,
                &site,
                &issue,
                yes_i_am_the_operator,
            )
            .await?;
            println!(
                "Administrator one-time secret (shown once, {}): {}",
                secret_note(&issue),
                secret.text()
            );
        }
        Command::Reload { site } => {
            let sites = match site {
                Some(site) => vec![site],
                None => spot_cli::config::load(&args.root, false)?
                    .sites
                    .into_iter()
                    .map(|s| s.site)
                    .collect(),
            };
            for site in &sites {
                spot_cli::request_reload(&args.root, site)?;
                println!("reload requested for site `{site}`");
            }
        }
        Command::Check => print!("{}", spot_cli::ops::check(&args.root).await?),
        Command::Serve { dev } => {
            let loaded = spot_cli::config::load(&args.root, dev)?;
            let _telemetry = spot_telemetry::init(loaded.common.telemetry.config())
                .map_err(|e| spot_cli::Error(e.to_string()))?;
            let handle = spot_cli::start(&args.root, dev, shutdown()).await?;
            handle
                .join()
                .await
                .map_err(|e| spot_cli::Error(e.to_string()))?;
        }
    }
    Ok(())
}
/// The instant the command runs: the one place the binary reads the clock for credentials.
fn issue_now() -> spot_cli::onetime::Issue {
    spot_cli::onetime::Issue::system()
}
/// What the operator must know about a one-time secret (never the secret itself).
fn secret_note(issue: &spot_cli::onetime::Issue) -> String {
    format!(
        "good for {} hours, only to choose a password, not an API key",
        (issue.secret_expires_at() - issue.issued_at()) / 3600
    )
}
async fn shutdown() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
