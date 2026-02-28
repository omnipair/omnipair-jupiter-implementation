use anchor_lang::prelude::{AccountMeta, AnchorDeserialize, Pubkey};
use anyhow::{Context, Result, bail};
use jupiter_amm_interface::{
    AccountMap, Amm, AmmContext, AmmProgramIdToLabel, KeyedAccount, Quote, Swap,
    SwapAndAccountMetas, SwapMode, SwapParams,
};
use rust_decimal::Decimal;

pub const OMNIPAIR_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("omnixgS8fnqHfCcTGKWj6JtKjzpJZ1Y5y9pyFkQDkYE");
pub const SPL_TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_2022_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

const BPS_DENOMINATOR: u128 = 10_000;
#[allow(dead_code)]
const PAIR_SEED_PREFIX: &[u8] = b"gamm_pair";
const RESERVE_VAULT_SEED_PREFIX: &[u8] = b"reserve_vault";
const FUTARCHY_AUTHORITY_SEED_PREFIX: &[u8] = b"futarchy_authority";

fn ceil_div(numerator: u128, denominator: u128) -> Option<u128> {
    if denominator == 0 {
        return None;
    }
    Some((numerator + denominator - 1) / denominator)
}

#[derive(Debug)]
pub enum OmnipairError {
    MathOverflow,
    InvalidReserves,
    InvalidQuoteParams,
    ExactOutNotSupported,
    InvalidAccountData,
    InsufficientCashReserve,
}

impl std::fmt::Display for OmnipairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(AnchorDeserialize, Debug, Clone, Copy, Default)]
pub struct VaultBumps {
    pub reserve0: u8,
    pub reserve1: u8,
    pub collateral0: u8,
    pub collateral1: u8,
}

#[derive(AnchorDeserialize, Debug, Clone, Copy, Default)]
pub struct LastPriceEMA {
    pub symmetric: u64,
    pub directional: u64,
}

#[derive(AnchorDeserialize, Debug, Clone)]
pub struct OmnipairPair {
    pub token0: Pubkey,
    pub token1: Pubkey,
    pub lp_mint: Pubkey,
    pub rate_model: Pubkey,
    pub swap_fee_bps: u16,
    pub half_life: u64,
    pub fixed_cf_bps: Option<u16>,
    pub reserve0: u64,
    pub reserve1: u64,
    pub cash_reserve0: u64,
    pub cash_reserve1: u64,
    pub last_price0_ema: LastPriceEMA,
    pub last_price1_ema: LastPriceEMA,
    pub last_update: u64,
    pub last_rate0: u64,
    pub last_rate1: u64,
    pub total_debt0: u64,
    pub total_debt1: u64,
    pub total_debt0_shares: u128,
    pub total_debt1_shares: u128,
    pub total_supply: u64,
    pub total_collateral0: u64,
    pub total_collateral1: u64,
    pub token0_decimals: u8,
    pub token1_decimals: u8,
    pub params_hash: [u8; 32],
    pub version: u8,
    pub bump: u8,
    pub vault_bumps: VaultBumps,
    pub reduce_only: bool,
}

impl OmnipairPair {
    fn calculate_amount_out(
        reserve_in: u64,
        reserve_out: u64,
        amount_in: u64,
    ) -> Result<u64> {
        let denominator = (reserve_in as u128)
            .checked_add(amount_in as u128)
            .ok_or_else(|| anyhow::anyhow!(OmnipairError::MathOverflow))?;
        let amount_out = (amount_in as u128)
            .checked_mul(reserve_out as u128)
            .ok_or_else(|| anyhow::anyhow!(OmnipairError::MathOverflow))?
            .checked_div(denominator)
            .ok_or_else(|| anyhow::anyhow!(OmnipairError::MathOverflow))?;
        Ok(amount_out as u64)
    }
}

#[derive(Debug, Clone)]
pub struct OmnipairAmmClient {
    pub pair_key: Pubkey,
    pub state: OmnipairPair,
}

impl AmmProgramIdToLabel for OmnipairAmmClient {
    const PROGRAM_ID_TO_LABELS: &[(Pubkey, jupiter_amm_interface::AmmLabel)] =
        &[(OMNIPAIR_PROGRAM_ID, "Omnipair")];
}

impl Amm for OmnipairAmmClient {
    fn label(&self) -> String {
        "Omnipair".to_string()
    }

    fn program_id(&self) -> Pubkey {
        OMNIPAIR_PROGRAM_ID
    }

    fn key(&self) -> Pubkey {
        self.pair_key
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        vec![self.state.token0, self.state.token1]
    }

    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        vec![self.pair_key]
    }

    fn update(&mut self, account_map: &AccountMap) -> Result<()> {
        let pair_account = account_map.get(&self.pair_key).with_context(|| {
            format!("Pair account not found for key: {}", self.pair_key)
        })?;

        if pair_account.data.len() < 8 {
            bail!(OmnipairError::InvalidAccountData);
        }

        let pair_data = OmnipairPair::deserialize(&mut &pair_account.data[8..])?;
        self.state = pair_data;

        Ok(())
    }

    fn get_accounts_len(&self) -> usize {
        14
    }

    fn from_keyed_account(keyed_account: &KeyedAccount, _amm_context: &AmmContext) -> Result<Self>
    where
        Self: Sized,
    {
        if keyed_account.account.data.len() < 8 {
            bail!(OmnipairError::InvalidAccountData);
        }

        let pair_data = OmnipairPair::deserialize(&mut &keyed_account.account.data[8..])?;

        Ok(Self {
            pair_key: keyed_account.key,
            state: pair_data,
        })
    }

    fn quote(&self, quote_params: &jupiter_amm_interface::QuoteParams) -> Result<Quote> {
        if quote_params.swap_mode == SwapMode::ExactOut {
            bail!(OmnipairError::ExactOutNotSupported);
        }

        let is_token0_in = quote_params.input_mint == self.state.token0;
        if !is_token0_in && quote_params.input_mint != self.state.token1 {
            bail!(OmnipairError::InvalidQuoteParams);
        }

        let (reserve_in, reserve_out, cash_reserve_out) = if is_token0_in {
            (self.state.reserve0, self.state.reserve1, self.state.cash_reserve1)
        } else {
            (self.state.reserve1, self.state.reserve0, self.state.cash_reserve0)
        };

        if reserve_in == 0 || reserve_out == 0 {
            bail!(OmnipairError::InvalidReserves);
        }

        let amount_in = quote_params.amount;

        let swap_fee = ceil_div(
            (amount_in as u128)
                .checked_mul(self.state.swap_fee_bps as u128)
                .ok_or_else(|| anyhow::anyhow!(OmnipairError::MathOverflow))?,
            BPS_DENOMINATOR,
        )
        .ok_or_else(|| anyhow::anyhow!(OmnipairError::MathOverflow))? as u64;

        let amount_in_after_fee = amount_in
            .checked_sub(swap_fee)
            .ok_or_else(|| anyhow::anyhow!(OmnipairError::MathOverflow))?;

        let amount_out =
            OmnipairPair::calculate_amount_out(reserve_in, reserve_out, amount_in_after_fee)?;

        if amount_out > cash_reserve_out {
            bail!(OmnipairError::InsufficientCashReserve);
        }

        let fee_pct = Decimal::new(self.state.swap_fee_bps as i64, 4);

        Ok(Quote {
            in_amount: amount_in,
            out_amount: amount_out,
            fee_amount: swap_fee,
            fee_mint: quote_params.input_mint,
            fee_pct,
        })
    }

    fn get_swap_and_account_metas(&self, swap_params: &SwapParams) -> Result<SwapAndAccountMetas> {
        let SwapParams {
            source_mint,
            destination_mint,
            destination_token_account,
            source_token_account,
            token_transfer_authority,
            ..
        } = swap_params;

        let is_token0_in = *source_mint == self.state.token0;

        let (token_in_mint, token_out_mint) = if is_token0_in {
            (self.state.token0, self.state.token1)
        } else {
            (self.state.token1, self.state.token0)
        };

        let (token_in_vault, _) = Pubkey::find_program_address(
            &[RESERVE_VAULT_SEED_PREFIX, self.pair_key.as_ref(), token_in_mint.as_ref()],
            &OMNIPAIR_PROGRAM_ID,
        );
        let (token_out_vault, _) = Pubkey::find_program_address(
            &[RESERVE_VAULT_SEED_PREFIX, self.pair_key.as_ref(), token_out_mint.as_ref()],
            &OMNIPAIR_PROGRAM_ID,
        );
        let (futarchy_authority, _) = Pubkey::find_program_address(
            &[FUTARCHY_AUTHORITY_SEED_PREFIX],
            &OMNIPAIR_PROGRAM_ID,
        );
        let (event_authority, _) = Pubkey::find_program_address(
            &[b"__event_authority"],
            &OMNIPAIR_PROGRAM_ID,
        );

        Ok(SwapAndAccountMetas {
            swap: Swap::TokenSwap,
            account_metas: OmnipairSwapAccounts {
                pair: self.pair_key,
                rate_model: self.state.rate_model,
                futarchy_authority,
                token_in_vault,
                token_out_vault,
                user_token_in_account: *source_token_account,
                user_token_out_account: *destination_token_account,
                token_in_mint,
                token_out_mint,
                user: *token_transfer_authority,
                token_program: SPL_TOKEN_PROGRAM_ID,
                token_2022_program: TOKEN_2022_PROGRAM_ID,
                event_authority,
                omnipair_program: OMNIPAIR_PROGRAM_ID,
            }
            .into(),
        })
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
        Box::new(self.clone())
    }
}

pub struct OmnipairSwapAccounts {
    pub pair: Pubkey,
    pub rate_model: Pubkey,
    pub futarchy_authority: Pubkey,
    pub token_in_vault: Pubkey,
    pub token_out_vault: Pubkey,
    pub user_token_in_account: Pubkey,
    pub user_token_out_account: Pubkey,
    pub token_in_mint: Pubkey,
    pub token_out_mint: Pubkey,
    pub user: Pubkey,
    pub token_program: Pubkey,
    pub token_2022_program: Pubkey,
    pub event_authority: Pubkey,
    pub omnipair_program: Pubkey,
}

impl From<OmnipairSwapAccounts> for Vec<AccountMeta> {
    fn from(accounts: OmnipairSwapAccounts) -> Self {
        vec![
            AccountMeta::new(accounts.pair, false),
            AccountMeta::new(accounts.rate_model, false),
            AccountMeta::new_readonly(accounts.futarchy_authority, false),
            AccountMeta::new(accounts.token_in_vault, false),
            AccountMeta::new(accounts.token_out_vault, false),
            AccountMeta::new(accounts.user_token_in_account, false),
            AccountMeta::new(accounts.user_token_out_account, false),
            AccountMeta::new_readonly(accounts.token_in_mint, false),
            AccountMeta::new_readonly(accounts.token_out_mint, false),
            AccountMeta::new(accounts.user, true),
            AccountMeta::new_readonly(accounts.token_program, false),
            AccountMeta::new_readonly(accounts.token_2022_program, false),
            AccountMeta::new_readonly(accounts.event_authority, false),
            AccountMeta::new_readonly(accounts.omnipair_program, false),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constant_product() {
        let out = OmnipairPair::calculate_amount_out(1_000_000, 1_000_000, 100_000).unwrap();
        assert_eq!(out, 90909);
    }

    #[test]
    fn test_quote_with_fee() {
        let pair = OmnipairPair {
            token0: Pubkey::new_unique(),
            token1: Pubkey::new_unique(),
            lp_mint: Pubkey::default(),
            rate_model: Pubkey::default(),
            swap_fee_bps: 30,
            half_life: 0,
            fixed_cf_bps: None,
            reserve0: 1_000_000_000,
            reserve1: 1_000_000_000,
            cash_reserve0: 1_000_000_000,
            cash_reserve1: 1_000_000_000,
            last_price0_ema: LastPriceEMA::default(),
            last_price1_ema: LastPriceEMA::default(),
            last_update: 0,
            last_rate0: 0,
            last_rate1: 0,
            total_debt0: 0,
            total_debt1: 0,
            total_debt0_shares: 0,
            total_debt1_shares: 0,
            total_supply: 0,
            total_collateral0: 0,
            total_collateral1: 0,
            token0_decimals: 9,
            token1_decimals: 9,
            params_hash: [0; 32],
            version: 1,
            bump: 0,
            vault_bumps: VaultBumps::default(),
            reduce_only: false,
        };

        let client = OmnipairAmmClient {
            pair_key: Pubkey::new_unique(),
            state: pair.clone(),
        };

        let quote = client
            .quote(&jupiter_amm_interface::QuoteParams {
                amount: 1_000_000,
                input_mint: pair.token0,
                output_mint: pair.token1,
                swap_mode: SwapMode::ExactIn,
            })
            .unwrap();

        assert_eq!(quote.fee_amount, 3000);
        assert_eq!(quote.out_amount, 996006);
        assert_eq!(quote.in_amount, 1_000_000);
    }

    /// Test against a live on-chain Omnipair Pair account.
    ///
    /// To run:
    ///   cargo test -p omnipair-amm-sdk test_onchain -- --nocapture --ignored
    ///
    /// Set env vars before running:
    ///   OMNIPAIR_RPC_URL  - Solana RPC endpoint (default: mainnet-beta)
    ///   OMNIPAIR_PAIR     - Pair account pubkey (required)
    #[test]
    #[ignore]
    fn test_onchain() {
        use solana_rpc_client::rpc_client::RpcClient;
        use solana_commitment_config::CommitmentConfig;
        use std::collections::HashMap;
        use std::str::FromStr;

        let rpc_url = std::env::var("OMNIPAIR_RPC_URL")
            .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
        let pair_str = std::env::var("OMNIPAIR_PAIR")
            .expect("Set OMNIPAIR_PAIR env var to a Pair account pubkey");
        let pair_pubkey = Pubkey::from_str(&pair_str).expect("Invalid OMNIPAIR_PAIR pubkey");

        let client = RpcClient::new_with_commitment(&rpc_url, CommitmentConfig::confirmed());

        // -- Deserialize --
        println!("Fetching pair account: {pair_pubkey}");
        let pair_account = client.get_account(&pair_pubkey).unwrap();
        println!("  owner:     {}", pair_account.owner);
        println!("  data len:  {} bytes", pair_account.data.len());
        assert_eq!(
            pair_account.owner, OMNIPAIR_PROGRAM_ID,
            "Account owner is not the Omnipair program"
        );

        let keyed_account = KeyedAccount {
            key: pair_pubkey,
            account: pair_account,
            params: None,
        };
        let amm_context = AmmContext {
            clock_ref: jupiter_amm_interface::ClockRef::default(),
        };
        let mut amm =
            OmnipairAmmClient::from_keyed_account(&keyed_account, &amm_context).unwrap();

        println!("\n=== Pair State ===");
        println!("  token0:          {}", amm.state.token0);
        println!("  token1:          {}", amm.state.token1);
        println!("  reserve0:        {}", amm.state.reserve0);
        println!("  reserve1:        {}", amm.state.reserve1);
        println!("  cash_reserve0:   {}", amm.state.cash_reserve0);
        println!("  cash_reserve1:   {}", amm.state.cash_reserve1);
        println!("  swap_fee_bps:    {}", amm.state.swap_fee_bps);
        println!("  rate_model:      {}", amm.state.rate_model);
        println!("  lp_mint:         {}", amm.state.lp_mint);
        println!("  total_debt0:     {}", amm.state.total_debt0);
        println!("  total_debt1:     {}", amm.state.total_debt1);
        println!("  total_supply:    {}", amm.state.total_supply);
        println!("  bump:            {}", amm.state.bump);
        println!("  version:         {}", amm.state.version);
        println!("  reduce_only:     {}", amm.state.reduce_only);

        // -- Update --
        println!("\n=== Update (re-fetch) ===");
        let accounts_to_update = amm.get_accounts_to_update();
        println!("  accounts to update: {:?}", accounts_to_update);
        let account_map: HashMap<Pubkey, solana_sdk::account::Account, ahash::RandomState> =
            client
                .get_multiple_accounts(&accounts_to_update)
                .unwrap()
                .into_iter()
                .zip(accounts_to_update)
                .filter_map(|(account, pubkey)| account.map(|a| (pubkey, a)))
                .collect();
        amm.update(&account_map).unwrap();
        println!("  update OK");

        // -- Quote token0 -> token1 --
        let amount_0 = 1000 * 10u64.pow(amm.state.token0_decimals as u32);
        println!("\n=== Quote: token0 -> token1 (amount_in = {amount_0}) ===");
        match amm.quote(&jupiter_amm_interface::QuoteParams {
            amount: amount_0,
            input_mint: amm.state.token0,
            output_mint: amm.state.token1,
            swap_mode: SwapMode::ExactIn,
        }) {
            Ok(quote) => {
                println!("  in_amount:   {}", quote.in_amount);
                println!("  out_amount:  {}", quote.out_amount);
                println!("  fee_amount:  {}", quote.fee_amount);
                println!("  fee_mint:    {}", quote.fee_mint);
                println!("  fee_pct:     {}", quote.fee_pct);
                assert!(quote.out_amount > 0, "out_amount should be > 0");
                assert!(quote.fee_amount > 0 || amm.state.swap_fee_bps == 0);
            }
            Err(e) => {
                println!("  quote failed: {e}");
                println!("  (this may be expected if cash_reserve is low)");
            }
        }

        // -- Quote token1 -> token0 --
        let amount_1 = 1000 * 10u64.pow(amm.state.token1_decimals as u32);
        println!("\n=== Quote: token1 -> token0 (amount_in = {amount_1}) ===");
        match amm.quote(&jupiter_amm_interface::QuoteParams {
            amount: amount_1,
            input_mint: amm.state.token1,
            output_mint: amm.state.token0,
            swap_mode: SwapMode::ExactIn,
        }) {
            Ok(quote) => {
                println!("  in_amount:   {}", quote.in_amount);
                println!("  out_amount:  {}", quote.out_amount);
                println!("  fee_amount:  {}", quote.fee_amount);
                println!("  fee_mint:    {}", quote.fee_mint);
                println!("  fee_pct:     {}", quote.fee_pct);
                assert!(quote.out_amount > 0, "out_amount should be > 0");
                assert!(quote.fee_amount > 0 || amm.state.swap_fee_bps == 0);
            }
            Err(e) => {
                println!("  quote failed: {e}");
                println!("  (this may be expected if cash_reserve is low)");
            }
        }

        // -- Verify swap account metas --
        println!("\n=== Swap Account Metas (token0 -> token1) ===");
        let placeholder = Pubkey::new_unique();
        let swap_and_metas = amm
            .get_swap_and_account_metas(&SwapParams {
                swap_mode: SwapMode::ExactIn,
                source_mint: amm.state.token0,
                destination_mint: amm.state.token1,
                source_token_account: placeholder,
                destination_token_account: placeholder,
                token_transfer_authority: placeholder,
                quote_mint_to_referrer: None,
                in_amount: amount_0,
                out_amount: 0,
                jupiter_program_id: &placeholder,
                missing_dynamic_accounts_as_default: false,
            })
            .unwrap();

        println!("  swap variant: {:?}", swap_and_metas.swap);
        println!("  num accounts: {}", swap_and_metas.account_metas.len());
        let labels = [
            "pair", "rate_model", "futarchy_authority", "token_in_vault",
            "token_out_vault", "user_token_in", "user_token_out",
            "token_in_mint", "token_out_mint", "user", "token_program",
            "token_2022_program", "event_authority", "omnipair_program",
        ];
        for (i, meta) in swap_and_metas.account_metas.iter().enumerate() {
            let label = labels.get(i).unwrap_or(&"?");
            let flags = format!(
                "{}{}",
                if meta.is_writable { "W" } else { "R" },
                if meta.is_signer { "S" } else { "" }
            );
            println!("  [{i:2}] {label:<20} {:<44} {flags}", meta.pubkey);
        }

        // Verify the vault PDAs exist on-chain
        let vault_metas: Vec<_> = swap_and_metas.account_metas[3..5].to_vec();
        println!("\n=== Verifying vault PDAs exist on-chain ===");
        for (i, meta) in vault_metas.iter().enumerate() {
            match client.get_account(&meta.pubkey) {
                Ok(acc) => println!(
                    "  vault{i} {} owner={} data_len={}",
                    meta.pubkey, acc.owner, acc.data.len()
                ),
                Err(e) => println!("  vault{i} {} NOT FOUND: {e}", meta.pubkey),
            }
        }

        println!("\n=== All checks passed ===");
    }
}
