use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;

use opencrab_cli_gateway::args::Args;
use opencrab_cli_gateway::config::Placement;
use opencrab_cli_gateway::runtime::{self, Control, Exit, RunOptions};
use tokio::sync::mpsc;

fn main() -> ExitCode {
    let args = match Args::parse(std::env::args()) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let placement = match Placement::load(&args.placement) {
        Ok(placement) => placement,
        Err(error) => {
            eprintln!("invalid placement: {error}");
            return ExitCode::from(2);
        }
    };
    let instance = match placement.select_agent(&args.agent) {
        Ok(instance) => instance.clone(),
        Err(error) => {
            eprintln!("invalid placement selection: {error}");
            return ExitCode::from(2);
        }
    };
    let frontend = match opencrab_cli_gateway::resolve_mode(
        args.mode,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    ) {
        Ok(frontend) => frontend,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("runtime failure: {error}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(async_main(args, placement, instance, frontend)) {
        Ok(Exit::Normal) => ExitCode::SUCCESS,
        Ok(Exit::Interrupted) => ExitCode::from(130),
        Err(error) => {
            eprintln!("gateway failure: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn async_main(
    args: Args,
    placement: Placement,
    instance: opencrab_cli_gateway::config::InstancePlacement,
    frontend: runtime::Frontend,
) -> anyhow::Result<Exit> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("opencrab_cli_gateway=info".parse()?)
                .add_directive("opencrab_gate_client=info".parse()?),
        )
        .init();
    let (control_tx, control_rx) = mpsc::channel(2);
    tokio::spawn(signal_owner(control_tx));
    runtime::run(
        RunOptions {
            placement,
            instance,
            session: args.session,
            frontend,
            connect_timeout: Duration::from_secs(args.connect_timeout_secs),
        },
        tokio::io::stdin(),
        tokio::io::stdout(),
        control_rx,
    )
    .await
}

async fn signal_owner(sender: mpsc::Sender<Control>) {
    let mut interrupt =
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()) {
            Ok(signal) => signal,
            Err(_) => return,
        };
    let mut terminate =
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(_) => return,
        };
    let first = tokio::select! {
        _ = interrupt.recv() => Control::Interrupt,
        _ = terminate.recv() => Control::Terminate,
    };
    if sender.send(first).await.is_err() {
        return;
    }
    tokio::select! {
        _ = interrupt.recv() => std::process::exit(130),
        _ = terminate.recv() => std::process::exit(1),
    }
}
