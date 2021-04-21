use sc_cli::{ChainSpec, RuntimeVersion, SubstrateCli};

use crate::cli::{Cli, Subcommand};
use crate::{chain_spec, service};

impl SubstrateCli for Cli {
	fn impl_name() -> String {
		"Patracts Dev Node".into()
	}

	fn impl_version() -> String {
		env!("SUBSTRATE_CLI_IMPL_VERSION").into()
	}

	fn description() -> String {
		env!("CARGO_PKG_DESCRIPTION").into()
	}

	fn author() -> String {
		env!("CARGO_PKG_AUTHORS").into()
	}

	fn support_url() -> String {
		"https://github.com/patractlabs/patracts".into()
	}

	fn copyright_start_year() -> i32 {
		2020
	}

	fn load_spec(&self, id: &str) -> Result<Box<dyn ec_service::ChainSpec>, String> {
		// todo chain_spec would receive some params to generate account or other thing dynamically,
		// maybe use some global vars or something others.
		Ok(match id {
			"dev" | _ => Box::new(chain_spec::development_config()?),
		})
	}

	fn native_runtime_version(_: &Box<dyn ChainSpec>) -> &'static RuntimeVersion {
		&dev_runtime::VERSION
	}
}

/// Parse and run command line arguments
pub fn run() -> sc_cli::Result<()> {
	let cli = Cli::from_args();

	match &cli.subcommand {
		Some(sub) => match sub {
			Subcommand::StateKv(cmd) => {
				let runner = ec_cli::build_runner(&cli, cmd)?;
				runner.sync_run(|config| {
					let state_kv = service::new_state_kv(&config, true)?;
					cmd.run::<dev_runtime::opaque::Block, _>(state_kv)
				})
			}
			Subcommand::Workspace(cmd) => cmd.init_and_run::<Cli>(),
			Subcommand::BuildSpec(cmd) => {
				let runner = cli.create_runner(cmd)?;
				runner.sync_run(|config| cmd.run(config.chain_spec, config.network))
			}
		},
		None => {
			let command = &cli.run;
			let runner = ec_cli::build_runner(&cli, command)?;
			runner.run_node_until_exit(|config| service::new_full(config))
		}
	}
}
