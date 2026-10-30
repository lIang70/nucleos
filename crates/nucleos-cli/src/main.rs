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
        Some("--help" | "-h") | None => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(Box::from(format!(
            "unknown command {other:?}\n\n{USAGE}"
        ))),
    }
}

const USAGE: &str = "\
usage: nucleos <command> [options]

  init                 create a deployment: stores, recovery password, administrator key
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
