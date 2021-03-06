#[macro_use]
extern crate log;

use clap::{App, AppSettings, Arg};
use env_logger::Env;
use nb::Node;
use tokio::runtime::Runtime;

fn main() {
    let matches = App::new("nb")
        .version(env!("CARGO_PKG_VERSION"))
        .author(env!("CARGO_PKG_AUTHORS"))
        .about("A simple blockchain node")
        .setting(AppSettings::DisableHelpSubcommand)
        .arg(
            Arg::with_name("addr")
                .long("addr")
                .takes_value(true)
                .value_name("IP-PORT")
                .default_value("127.0.0.1:4000")
                .help("the node's address"),
        )
        .get_matches();

    let addr = matches.value_of("addr").unwrap();
    let addr = addr.to_owned();
    env_logger::from_env(Env::default().default_filter_or("debug")).init();

    info!("nb {}", env!("CARGO_PKG_VERSION"));
    info!("Listening on {}", addr);

    let rt = Runtime::new().expect("tokio runtime can be initialized");
    rt.block_on(async move { Node::handle_events(addr).await.unwrap() });
}
