use hex_literal::hex;
use sc_telemetry::serde_json::json;

use core_primitives::{AccountId, Signature};
use patracts_runtime::{
	AuraConfig, BalancesConfig, ContractsConfig, GenesisConfig, GrandpaConfig, SudoConfig,
	SystemConfig, WASM_BINARY,
};
use sc_service::ChainType;
use sp_consensus_aura::sr25519::AuthorityId as AuraId;
use sp_core::{crypto::UncheckedInto, sr25519, Pair, Public};
use sp_finality_grandpa::AuthorityId as GrandpaId;
use sp_runtime::traits::{IdentifyAccount, Verify};

// The URL for the telemetry server.
// const STAGING_TELEMETRY_URL: &str = "wss://telemetry.polkadot.io/submit/";

/// Specialized `ChainSpec`. This is a specialization of the general Substrate ChainSpec type.
pub type ChainSpec = sc_service::GenericChainSpec<GenesisConfig>;

/// Generate a crypto pair from seed.
pub fn get_from_seed<TPublic: Public>(seed: &str) -> <TPublic::Pair as Pair>::Public {
	TPublic::Pair::from_string(&format!("//{}", seed), None)
		.expect("static values are valid; qed")
		.public()
}

type AccountPublic = <Signature as Verify>::Signer;

/// Generate an account ID from seed.
pub fn get_account_id_from_seed<TPublic: Public>(seed: &str) -> AccountId
where
	AccountPublic: From<<TPublic::Pair as Pair>::Public>,
{
	AccountPublic::from(get_from_seed::<TPublic>(seed)).into_account()
}

/// Generate an Aura authority key.
pub fn authority_keys_from_seed(s: &str) -> (AuraId, GrandpaId) {
	(get_from_seed::<AuraId>(s), get_from_seed::<GrandpaId>(s))
}

pub fn development_config() -> Result<ChainSpec, String> {
	let wasm_binary = WASM_BINARY.ok_or_else(|| "Development wasm not available".to_string())?;

	Ok(ChainSpec::from_genesis(
		// Name
		"Development",
		// ID
		"dev",
		ChainType::Development,
		move || {
			testnet_genesis(
				wasm_binary,
				// Initial PoA authorities
				vec![authority_keys_from_seed("Alice")],
				// Sudo account
				get_account_id_from_seed::<sr25519::Public>("Alice"),
				// Pre-funded accounts
				vec![
					get_account_id_from_seed::<sr25519::Public>("Alice"),
					get_account_id_from_seed::<sr25519::Public>("Bob"),
					get_account_id_from_seed::<sr25519::Public>("Alice//stash"),
					get_account_id_from_seed::<sr25519::Public>("Bob//stash"),
				],
				true,
			)
		},
		// Bootnodes
		vec![],
		// Telemetry
		None,
		// Protocol ID
		None,
		// Properties
		None,
		// Extensions
		None,
	))
}

pub fn local_testnet_config() -> Result<ChainSpec, String> {
	let wasm_binary = WASM_BINARY.ok_or_else(|| "Development wasm not available".to_string())?;

	Ok(ChainSpec::from_genesis(
		// Name
		"Local Testnet",
		// ID
		"local_testnet",
		ChainType::Local,
		move || {
			testnet_genesis(
				wasm_binary,
				// Initial PoA authorities
				vec![
					authority_keys_from_seed("Alice"),
					authority_keys_from_seed("Bob"),
				],
				// Sudo account
				get_account_id_from_seed::<sr25519::Public>("Alice"),
				// Pre-funded accounts
				vec![
					get_account_id_from_seed::<sr25519::Public>("Alice"),
					get_account_id_from_seed::<sr25519::Public>("Bob"),
					get_account_id_from_seed::<sr25519::Public>("Charlie"),
					get_account_id_from_seed::<sr25519::Public>("Dave"),
					get_account_id_from_seed::<sr25519::Public>("Eve"),
					get_account_id_from_seed::<sr25519::Public>("Ferdie"),
					get_account_id_from_seed::<sr25519::Public>("Alice//stash"),
					get_account_id_from_seed::<sr25519::Public>("Bob//stash"),
					get_account_id_from_seed::<sr25519::Public>("Charlie//stash"),
					get_account_id_from_seed::<sr25519::Public>("Dave//stash"),
					get_account_id_from_seed::<sr25519::Public>("Eve//stash"),
					get_account_id_from_seed::<sr25519::Public>("Ferdie//stash"),
				],
				true,
			)
		},
		// Bootnodes
		vec![],
		// Telemetry
		None,
		// Protocol ID
		None,
		// Properties
		None,
		// Extensions
		None,
	))
}

/// Configure initial storage state for FRAME modules.
fn testnet_genesis(
	wasm_binary: &[u8],
	initial_authorities: Vec<(AuraId, GrandpaId)>,
	root_key: AccountId,
	endowed_accounts: Vec<AccountId>,
	enable_println: bool,
) -> GenesisConfig {
	GenesisConfig {
		frame_system: SystemConfig {
			// Add Wasm runtime to storage.
			code: wasm_binary.to_vec(),
			changes_trie_config: Default::default(),
		},
		pallet_balances: BalancesConfig {
			// Configure endowed accounts with initial balance of 1 << 60.
			balances: endowed_accounts
				.iter()
				.cloned()
				.map(|k| (k, 1 << 60))
				.collect(),
		},
		pallet_aura: AuraConfig {
			authorities: initial_authorities.iter().map(|x| (x.0.clone())).collect(),
		},
		pallet_grandpa: GrandpaConfig {
			authorities: initial_authorities
				.iter()
				.map(|x| (x.1.clone(), 1))
				.collect(),
		},
		pallet_sudo: SudoConfig {
			// Assign network admin rights.
			key: root_key,
		},
		pallet_contracts: ContractsConfig {
			// println should only be enabled on development chains
			current_schedule: pallet_contracts::Schedule::default().enable_println(enable_println),
		},
	}
}

pub fn staging_testnet_config_genesis() -> Result<ChainSpec, String> {
	let wasm_binary = WASM_BINARY.ok_or("Testnet wasm binary not available".to_string())?;

	// export secret="crush index company taste champion chat execute armor recipe pear spirit wise"
	// aura, grandpa
	// generated with secret:
	// for i in 1 2 ; do for j in aura; do subkey inspect "$secret"//$j//$i; done; done
	// and
	// for i in 1 2 ; do for j in gran; do subkey inspect --scheme ed25519 "$secret"//$j//$i; done; done

	let initial_authorities: Vec<(AuraId, GrandpaId)> = vec![
		(
			// 5Cz4oEV1QvkqFP5mBUB4wQPrcXqNiALsz3HZNFBcJHXZtLKc
			hex!["28b27fd4b0f367cb954ea96d678213e993816e68a3a5603939da40c455cb1135"]
				.unchecked_into(),
			// 5CBat2tuQwyKHWPb9jK8QNcqxFMWuyXHRk7m7stoXnipfwSo
			hex!["053f3ba1e04e2c63251a9f344f3b7cdff4d0778dad869e35272ded2baec127b3"]
				.unchecked_into(),
		),
		(
			// 5GQsd2ZMPq9C4ZbZ3Biz672fh7fGrur6doXTXKH1d85GSoK9
			hex!["c052b692bc362ad539b71623b2c589d6cdfe4d4e7ad5c2fbe974997c5739022f"]
				.unchecked_into(),
			// 5Fad4hv4NtdULxkV4TNdMEEA5KrM8jU62oec9uDZXW3ZHPTu
			hex!["9b85edfcf99dc337fd304827ec89ffa54dafa26ed2e2244d563fb1ff99b5a867"]
				.unchecked_into(),
		),
	];

	// generated with secret: subkey inspect "$secret"
	let root_key: AccountId = hex![
		// 5G6pXvDeXvrU6VMEtddff9efZkNnUk1tBBbJLYzZ5pjtCUxW
		"b28de6843f00ac3963fbd5edb234fc0bad58de623e4b668407ba4ff2bdf07151"
	]
	.into();

	let endowed_accounts: Vec<AccountId> = vec![root_key.clone()];

	Ok(ChainSpec::from_genesis(
		// Name
		"Patracts Deposit Staging",
		// ID
		"patracts_deposit_staging",
		ChainType::Live,
		move || {
			testnet_genesis(
				wasm_binary,
				initial_authorities.clone(),
				root_key.clone(),
				endowed_accounts.clone(),
				false,
			)
		},
		// Bootnodes
		vec![],
		// Telemetry
		None,
		// Protocol ID
		Some("patracts_deposit_staging"),
		// Properties
		Some(
			json!({
				"ss58Format": patracts_runtime::SS58Prefix::get(),
				"tokenDecimals": 10,
				"tokenSymbol": "DOT"
			})
			.as_object()
			.expect("network properties generation can not fail; qed")
			.to_owned(),
		),
		// Extensions
		None,
	))
}
