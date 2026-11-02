//! The `nucleos` command line.
//!
//! One subcommand so far: `init`. It is the only command that touches this machine's files; everything
//! after it is a signed request to the daemon, which is why `init` is also the command that creates the
//! credential those requests will be signed with.

use nucleos_cli::init::{Options, run};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("init") => init(&args[1..]),
        Some("config") => config(&args[1..]),
        Some("models") => models(&args[1..]),
        Some("tokens") => tokens(&args[1..]),
        Some("sources") => sources(&args[1..]),
        Some("admin") => admin(&args[1..]),
        Some("--help" | "-h") | None => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(Box::from(format!("unknown command {other:?}\n\n{USAGE}"))),
    }
}

const USAGE: &str = "\
usage: nucleos <command> [options]

  init                 create a deployment: stores, recovery password, administrator key
  config               print what the daemon is running (signs the request)
  models list          the models this deployment has
  models add           add or replace one       (--name --url --model [--key-file] [--context]
                                                 [--strict] [--reasoning] [--api-key-stdin])
  models remove <name> remove one
  tokens add           issue a tenant token  (--tenant --subject); printed once
  tokens list          the tokens this deployment has
  tokens remove <id>   revoke one, effective on the next request
  sources list         the MCP servers this deployment runs
  sources add          run one            (--name --command [--arg …] [--env K=V …])
  sources remove <name> stop one, publishing a generation without its tools
  admin recover        enrol a new administrator key using the recovery password
  --help               this text

init options:
  --root <dir>         where the stores live      (default: /var/lib/nucleos, or
                                                   /Library/Application Support/nucleos)
  --socket <path>      where the daemon will listen
  --tenant <name>      the first administrator's tenant      (default: default)
  --subject <name>     the first administrator's subject     (default: admin)
  --password-stdin     read the recovery password from stdin instead of prompting
  --key-out <path>     write the private key to a file instead of the keychain
  --no-client-config   do not write the socket discovery file
  --force              replace an existing deployment

remote options (config and models):
  --socket <path>      where the daemon listens     (default: the client config)
  --key-id <id>        which key signs               (default: the client config)
  --key-file <path>    read the private key from a file instead of the keychain

The private key is the one thing that is never written into the stores: it goes to the operating
system's keychain, or to the file named by --key-out for a machine without a login session.";

fn init(args: &[String]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut root, mut socket, mut tenant, mut subject) = (None, None, None, None);
    let (mut password_stdin, mut key_out, mut client_config) = (false, None, true);
    let mut force = false;

    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let mut value = |what: &str| -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
            rest.next()
                .cloned()
                .ok_or_else(|| Box::from(format!("{what} needs a value")))
        };
        match arg.as_str() {
            "--root" => root = Some(PathBuf::from(value("--root")?)),
            "--socket" => socket = Some(PathBuf::from(value("--socket")?)),
            "--tenant" => tenant = Some(value("--tenant")?),
            "--subject" => subject = Some(value("--subject")?),
            "--key-out" => key_out = Some(PathBuf::from(value("--key-out")?)),
            "--password-stdin" => password_stdin = true,
            "--no-client-config" => client_config = false,
            "--force" => force = true,
            other => return Err(Box::from(format!("unknown option {other:?}\ntry --help"))),
        }
    }

    let layout = nucleosd_layout(root, socket);
    let password = if password_stdin {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        line.trim_end_matches(['\n', '\r']).to_owned()
    } else {
        let first = rpassword::prompt_password("recovery password: ")?;
        let again = rpassword::prompt_password("repeat: ")?;
        if first != again {
            return Err(Box::from("the passwords do not match"));
        }
        first
    };

    let mut options = Options::new(layout.0, layout.1, password);
    options.key_out = key_out;
    options.force = force;
    if let Some(tenant) = tenant {
        options.tenant = tenant;
    }
    if let Some(subject) = subject {
        options.subject = subject;
    }
    if !client_config {
        options.client_config = None;
    }

    let runtime = tokio::runtime::Runtime::new()?;
    let done = runtime.block_on(run(&options))?;

    println!("✓ root            {}", options.root.display());
    println!("✓ socket          {}", options.socket.display());
    println!("✓ admin key       {} ({})", done.key_id, done.key_store);
    println!("✓ recovery        stored, argon2id");
    // Said out loud because the default location is the one place the design says it should not be:
    // a backup of the data directory would carry the key that makes the backup useless.
    println!(
        "✓ seal key        {} (0600 — move it out of the data directory for a real deployment)",
        options.root.join("seal.key").display()
    );
    println!("✓ bootstrap policy deny by default; this administrator may add models and sources");
    println!("\nnext: start nucleosd, then `nucleos models add`");
    Ok(())
}

/// The two paths a deployment is described by.
///
/// Duplicated from `nucleosd`'s layout rather than depended on: the CLI must not link the daemon just
/// to learn where a directory is, and the defaults are four lines of convention. The duplication is
/// the kind that fails loudly — a wrong default puts the stores where the daemon will not look, and
/// `--root` is right there in both `--help`s.
fn nucleosd_layout(root: Option<PathBuf>, socket: Option<PathBuf>) -> (PathBuf, PathBuf) {
    let root = root.unwrap_or_else(|| {
        if cfg!(target_os = "macos") {
            PathBuf::from("/Library/Application Support/nucleos")
        } else {
            PathBuf::from("/var/lib/nucleos")
        }
    });
    let socket = socket.unwrap_or_else(|| {
        if cfg!(target_os = "macos") {
            std::env::var_os("TMPDIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join("nucleos/nucleosd.sock")
        } else {
            std::env::var_os("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join("nucleos/nucleosd.sock")
        }
    });
    (root, socket)
}

fn config(args: &[String]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut options = nucleos_cli::config::Options::default();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let mut value = |what: &str| -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
            rest.next()
                .cloned()
                .ok_or_else(|| Box::from(format!("{what} needs a value")))
        };
        match arg.as_str() {
            "--socket" => options.socket = Some(PathBuf::from(value("--socket")?)),
            "--key-id" => options.key_id = Some(value("--key-id")?),
            "--key-file" => options.key_file = Some(PathBuf::from(value("--key-file")?)),
            other => return Err(Box::from(format!("unknown option {other:?}\ntry --help"))),
        }
    }
    nucleos_cli::config::run(&options)?;
    Ok(())
}

fn models(args: &[String]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (command, rest) = match args.split_first() {
        Some((command, rest)) => (command.as_str(), rest),
        None => return Err(Box::from("models needs a subcommand: list, add or remove")),
    };

    let mut overrides = nucleos_cli::remote::Overrides::default();
    let mut request = nucleos::wire::api::ModelRequest::default();
    let mut positional: Option<String> = None;

    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        let mut value = |what: &str| -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
            iter.next()
                .cloned()
                .ok_or_else(|| Box::from(format!("{what} needs a value")))
        };
        match arg.as_str() {
            "--socket" => overrides.socket = Some(PathBuf::from(value("--socket")?)),
            "--key-id" => overrides.key_id = Some(value("--key-id")?),
            "--key-file" => overrides.key_file = Some(PathBuf::from(value("--key-file")?)),
            "--name" => request.name = value("--name")?,
            "--url" => request.url = value("--url")?,
            "--model" => request.model = value("--model")?,
            "--key" => request.key_file = Some(value("--key")?),
            "--context" => request.context_window = Some(value("--context")?.parse()?),
            "--strict" => request.strict_tools = true,
            "--api-key-stdin" => {
                // Never from argv: `ps -e` lists arguments, and a credential in a process table is a
                // credential in a log somewhere.
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                request.key = Some(line.trim().to_owned());
            }
            "--reasoning" => request.reasoning = true,
            other if other.starts_with('-') => {
                return Err(Box::from(format!("unknown option {other:?}")))
            }
            other => positional = Some(other.to_owned()),
        }
    }

    let text = match command {
        "list" => nucleos_cli::models::list(&overrides)?,
        "add" => nucleos_cli::models::add(&overrides, &request)?,
        "remove" => {
            let name = positional.ok_or_else(|| Box::<dyn std::error::Error + Send + Sync>::from(
                "models remove needs a name",
            ))?;
            nucleos_cli::models::remove(&overrides, &name)?
        }
        other => return Err(Box::from(format!("unknown models subcommand {other:?}"))),
    };
    println!("{text}");
    Ok(())
}

fn tokens(args: &[String]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (command, rest) = match args.split_first() {
        Some((command, rest)) => (command.as_str(), rest),
        None => return Err(Box::from("tokens needs a subcommand: add, list or remove")),
    };

    let mut overrides = nucleos_cli::remote::Overrides::default();
    let (mut tenant, mut subject) = (String::new(), String::new());
    let mut positional: Option<String> = None;

    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        let mut value = |what: &str| -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
            iter.next()
                .cloned()
                .ok_or_else(|| Box::from(format!("{what} needs a value")))
        };
        match arg.as_str() {
            "--socket" => overrides.socket = Some(PathBuf::from(value("--socket")?)),
            "--key-id" => overrides.key_id = Some(value("--key-id")?),
            "--key-file" => overrides.key_file = Some(PathBuf::from(value("--key-file")?)),
            "--tenant" => tenant = value("--tenant")?,
            "--subject" => subject = value("--subject")?,
            other if other.starts_with('-') => {
                return Err(Box::from(format!("unknown option {other:?}")))
            }
            other => positional = Some(other.to_owned()),
        }
    }

    let text = match command {
        "add" => nucleos_cli::tokens::add(&overrides, &tenant, &subject)?,
        "list" => nucleos_cli::tokens::list(&overrides)?,
        "remove" => {
            let id = positional.ok_or_else(|| {
                Box::<dyn std::error::Error + Send + Sync>::from("tokens remove needs an id")
            })?;
            nucleos_cli::tokens::remove(&overrides, &id)?
        }
        other => return Err(Box::from(format!("unknown tokens subcommand {other:?}"))),
    };
    println!("{text}");
    Ok(())
}

fn sources(args: &[String]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (command, rest) = match args.split_first() {
        Some((command, rest)) => (command.as_str(), rest),
        None => return Err(Box::from("sources needs a subcommand: add, list or remove")),
    };

    let mut overrides = nucleos_cli::remote::Overrides::default();
    let mut request = nucleos::wire::api::SourceRequest::default();
    let mut positional: Option<String> = None;

    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        let mut value = |what: &str| -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
            iter.next()
                .cloned()
                .ok_or_else(|| Box::from(format!("{what} needs a value")))
        };
        match arg.as_str() {
            "--socket" => overrides.socket = Some(PathBuf::from(value("--socket")?)),
            "--key-id" => overrides.key_id = Some(value("--key-id")?),
            "--key-file" => overrides.key_file = Some(PathBuf::from(value("--key-file")?)),
            "--name" => request.name = value("--name")?,
            "--command" => request.command = value("--command")?,
            "--arg" => request.args.push(value("--arg")?),
            "--env" => {
                let pair = value("--env")?;
                let (key, value) = pair.split_once('=').ok_or_else(|| {
                    Box::<dyn std::error::Error + Send + Sync>::from("--env wants KEY=VALUE")
                })?;
                request.env.insert(key.to_owned(), value.to_owned());
            }
            other if other.starts_with('-') => {
                return Err(Box::from(format!("unknown option {other:?}")))
            }
            other => positional = Some(other.to_owned()),
        }
    }

    let text = match command {
        "add" => nucleos_cli::sources::add(&overrides, &request)?,
        "list" => nucleos_cli::sources::list(&overrides)?,
        "remove" => {
            let name = positional.ok_or_else(|| {
                Box::<dyn std::error::Error + Send + Sync>::from("sources remove needs a name")
            })?;
            nucleos_cli::sources::remove(&overrides, &name)?
        }
        other => return Err(Box::from(format!("unknown sources subcommand {other:?}"))),
    };
    println!("{text}");
    Ok(())
}

/// `nucleos admin recover`: the local command that uses the recovery password.
///
/// Local on purpose: it reads the store from the filesystem and never talks to the daemon. A remote
/// recovery path would be an oracle for guessing the one credential that can recreate administration.
fn admin(args: &[String]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (command, rest) = match args.split_first() {
        Some((command, rest)) => (command.as_str(), rest),
        None => return Err(Box::from("admin needs a subcommand: recover")),
    };
    if command != "recover" {
        return Err(Box::from(format!("unknown admin subcommand {command:?}")));
    }

    let mut root = None;
    let mut key_out = None;
    let mut tenant = None;
    let mut subject = None;
    let mut password_stdin = false;

    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        let mut value = |what: &str| -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
            iter.next()
                .cloned()
                .ok_or_else(|| Box::from(format!("{what} needs a value")))
        };
        match arg.as_str() {
            "--root" => root = Some(PathBuf::from(value("--root")?)),
            "--key-out" => key_out = Some(PathBuf::from(value("--key-out")?)),
            "--tenant" => tenant = Some(value("--tenant")?),
            "--subject" => subject = Some(value("--subject")?),
            "--password-stdin" => password_stdin = true,
            other => return Err(Box::from(format!("unknown option {other:?}"))),
        }
    }

    let password = if password_stdin {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        line.trim_end_matches(['\n', '\r']).to_owned()
    } else {
        rpassword::prompt_password("recovery password: ")?
    };

    let layout = nucleosd_layout(root, None);
    let mut options = nucleos_cli::admin::Options::new(layout.0, password);
    options.key_out = key_out;
    if let Some(tenant) = tenant {
        options.tenant = tenant;
    }
    if let Some(subject) = subject {
        options.subject = subject;
    }

    let runtime = tokio::runtime::Runtime::new()?;
    let done = runtime.block_on(nucleos_cli::admin::recover(&options))?;
    println!("✓ new admin key   {} ({})", done.key_id, done.key_store);
    println!("✓ {} key(s) enrolled", done.enrolled);
    println!(
        "\nno restart needed: the daemon re-reads the key list on every request, so this key works now"
    );
    Ok(())
}
