//! `jolta-mcp`: connects an agent's stdio to the MCP server inside Jolta.app.
//! It sends the agent's token from `JOLTA_MCP_TOKEN` as a first line, then
//! copies bytes between the two and does nothing else, so it never reads or
//! changes the agent's messages. It never sees account secrets.

use std::path::PathBuf;
use std::process::ExitCode;

use tokio::io::{AsyncWriteExt, stdin, stdout};
use tokio::net::UnixStream;

const GROUP: &str = "RWQLHW7V7V.app.jolta";

/// The socket in the app group container. Inside the sandbox HOME is this
/// tool's own container, so the user's home is the part before it.
fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("JOLTA_MCP_SOCKET") {
        return p.into();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let home = home.split("/Library/Containers/").next().unwrap_or(&home).to_string();
    PathBuf::from(home).join("Library/Group Containers").join(GROUP).join("jolta.sock")
}

/// The line that tells Jolta which agent this is. Settings › Agents issues
/// tokens of URL-safe base64, which need no JSON escaping.
fn token_line() -> Result<Option<String>, String> {
    let Ok(token) = std::env::var("JOLTA_MCP_TOKEN") else { return Ok(None) };
    let token = token.trim();
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err("JOLTA_MCP_TOKEN isn't a token from Jolta. Copy it again from Settings › Agents.".into());
    }
    Ok(Some(format!("{{\"jsonrpc\":\"2.0\",\"method\":\"jolta/token\",\"params\":{{\"token\":\"{token}\"}}}}\n")))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let token = match token_line() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("jolta-mcp: {e}");
            return ExitCode::FAILURE;
        }
    };
    let path = socket_path();
    let stream = match UnixStream::connect(&path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("jolta-mcp: Jolta is not running or its MCP server is off ({}: {e}). Open Jolta and try again.", path.display());
            return ExitCode::FAILURE;
        }
    };
    let (mut from_jolta, mut to_jolta) = stream.into_split();
    let upstream = async {
        if let Some(line) = &token {
            to_jolta.write_all(line.as_bytes()).await?;
        }
        tokio::io::copy(&mut stdin(), &mut to_jolta).await?;
        to_jolta.shutdown().await
    };
    let downstream = async {
        tokio::io::copy(&mut from_jolta, &mut stdout()).await?;
        stdout().flush().await
    };
    match tokio::try_join!(upstream, downstream) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("jolta-mcp: {e}");
            ExitCode::FAILURE
        }
    }
}
