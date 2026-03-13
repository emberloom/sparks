Kill all running Emberloom processes, rebuild with the telegram feature, and restart both the telegram bot and observer.

Steps:
1. Run `pkill -f "target/debug/athena"` to kill existing processes (ignore errors if none running)
2. Run `cargo build --features telegram` and verify it succeeds
3. Start both processes in background with nohup:
   - `RUST_LOG=athena=info nohup ./target/debug/athena telegram > /tmp/athena_telegram.log 2>&1 &`
   - `RUST_LOG=athena=info nohup ./target/debug/athena observe > /tmp/athena_observe.log 2>&1 &`
4. Wait 2 seconds, then verify both processes are running with `pgrep -fa "target/debug/athena"`
5. Show the last 5 lines of /tmp/athena_telegram.log to confirm startup

Report success or failure concisely.
