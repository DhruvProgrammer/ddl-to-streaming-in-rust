//! Binary entry point. All logic lives in the library so integration tests can
//! exercise the real server without spawning a process.

use ddl_player::shutdown_token;

fn main() -> std::io::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("ddl-worker")
        .build()?;
    rt.block_on(async move {
        let shutdown = shutdown_token();
        let serve = ddl_player::main();
        tokio::select! {
            r = serve => r,
            _ = shutdown.cancelled() => Ok(()),
        }
    })
}
