//! `kite-mcp`: connects an agent's stdio to the MCP server inside Kite.app.
//! It copies bytes between the two and does nothing else; it never sees
//! account secrets.

use std::path::PathBuf;
use std::process::ExitCode;

use tokio::io::{AsyncWriteExt, stdin, stdout};
use tokio::net::UnixStream;

const GROUP: &str = "RWQLHW7V7V.be.example.kite";

/// The socket in the app group container. Inside the sandbox HOME is this
/// tool's own container, so the user's home is the part before it.
fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("KITE_MCP_SOCKET") {
        return p.into();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let home = home.split("/Library/Containers/").next().unwrap_or(&home).to_string();
    PathBuf::from(home).join("Library/Group Containers").join(GROUP).join("kite.sock")
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let path = socket_path();
    let stream = match UnixStream::connect(&path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("kite-mcp: Kite is not running or its MCP server is off ({}: {e}). Open Kite and try again.", path.display());
            return ExitCode::FAILURE;
        }
    };
    let (mut from_kite, mut to_kite) = stream.into_split();
    let upstream = async {
        tokio::io::copy(&mut stdin(), &mut to_kite).await?;
        to_kite.shutdown().await
    };
    let downstream = async {
        tokio::io::copy(&mut from_kite, &mut stdout()).await?;
        stdout().flush().await
    };
    match tokio::try_join!(upstream, downstream) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kite-mcp: {e}");
            ExitCode::FAILURE
        }
    }
}
