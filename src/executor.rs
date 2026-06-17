use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolCall;
use std::str::FromStr;
use tracing::{error, info, warn};

use crate::error::{AppError, AppResult};
use crate::models::Authorization;

// ---------------------------------------------------------------------------
// Morpho Blue Contract Interface
// ---------------------------------------------------------------------------

alloy::sol! {
    /// Authorization tuple for Morpho Blue.
    #[allow(missing_docs)]
    struct AuthorizationTuple {
        address authorizer;
        address authorized;
        bool isAuthorized;
        uint256 nonce;
        uint256 deadline;
    }

    /// MarketParams for Morpho Blue.
    #[allow(missing_docs)]
    struct MarketParams {
        bytes32 loanToken;
        bytes32 collateralToken;
        address oracle;
        address irm;
        uint256 lltv;
    }

    /// Call3 struct for Multicall3.
    #[allow(missing_docs)]
    struct Call3 {
        address target;
        bool allowFailure;
        bytes callData;
    }

    /// Result3 struct for Multicall3.
    #[allow(missing_docs)]
    struct Result3 {
        bool success;
        bytes returnData;
    }

    /// Minimal Morpho Blue interface.
    #[allow(missing_docs)]
    interface IMorphoBlue {
        function setAuthorizationWithSignature(
            AuthorizationTuple authorization,
            bytes signature
        ) external;

        function withdrawCollateral(
            MarketParams marketParams,
            uint256 assets,
            address onBehalf,
            address receiver
        ) external returns (uint256 assetsWithdrawn);
    }

    /// Multicall3 interface for atomic execution.
    #[allow(missing_docs)]
    interface IMulticall3 {
        function aggregate3(Call3[] calls)
            external
            returns (Result3[]);
    }
}

// ---------------------------------------------------------------------------
// Bot Executor
// ---------------------------------------------------------------------------

/// The bot executor handles atomic execution of emergency withdrawals.
#[derive(Clone)]
pub struct BotExecutor {
    /// Hot wallet signer address
    pub signer_address: Address,
    /// Hot wallet private key (as hex string; owned by the executing task)
    pub private_key_hex: String,
    /// Morpho Blue contract address
    pub morpho_blue: Address,
    /// Multicall3 contract address
    pub multicall3: Address,
    /// Chain RPC URL
    pub rpc_url: String,
    /// Flashbots RPC URL (optional)
    pub flashbots_url: Option<String>,
    /// Minimum ETH balance before warning
    pub gas_min_balance: U256,
}

impl BotExecutor {
    /// Default Multicall3 address (same on all EVM chains).
    pub const MULTICALL3: &str = "0xcA11bde33A8E2b1ad6bf053c61E3A8e7e2A9d1E9";

    /// Create a new bot executor.
    pub fn new(
        private_key_hex: &str,
        morpho_blue: Address,
        rpc_url: &str,
        flashbots_url: Option<&str>,
        gas_min_balance: U256,
    ) -> AppResult<Self> {
        // Derive address from private key
        let signer = PrivateKeySigner::from_str(private_key_hex).map_err(|e| {
            AppError::Config(format!("Invalid hot wallet private key: {}", e))
        })?;

        // Parse Multicall3 address
        let multicall3: Address = Self::MULTICALL3.parse().map_err(|e| {
            AppError::Config(format!("Invalid Multicall3 address: {}", e))
        })?;

        Ok(Self {
            signer_address: signer.address(),
            private_key_hex: private_key_hex.to_string(),
            morpho_blue,
            multicall3,
            rpc_url: rpc_url.to_string(),
            flashbots_url: flashbots_url.map(|s| s.to_string()),
            gas_min_balance,
        })
    }

    /// Check the hot wallet's ETH balance and warn if low.
    pub async fn check_balance(&self) -> AppResult<U256> {
        let url = self.rpc_url.parse().map_err(|e| {
            AppError::Config(format!("Invalid RPC URL: {}", e))
        })?;

        let provider = ProviderBuilder::new().connect_http(url);

        let balance = provider
            .get_balance(self.signer_address)
            .await
            .map_err(|e| AppError::RpcError(format!("Failed to get balance: {}", e)))?;

        if balance < self.gas_min_balance {
            warn!(
                "Hot wallet balance ({}) is below minimum ({})",
                balance, self.gas_min_balance
            );
        }

        Ok(balance)
    }

    /// Build the calldata for `setAuthorizationWithSignature`.
    pub fn build_auth_calldata(
        authorization: &Authorization,
        signature: &str,
    ) -> AppResult<Bytes> {
        let sig_bytes = hex::decode(signature.trim_start_matches("0x")).map_err(|e| {
            AppError::Execution(format!("Invalid signature hex: {}", e))
        })?;

        let sig_bytes = Bytes::from(sig_bytes);

        let auth_tuple = AuthorizationTuple {
            authorizer: authorization.authorizer,
            authorized: authorization.authorized,
            isAuthorized: authorization.is_authorized,
            nonce: authorization.nonce,
            deadline: authorization.deadline,
        };

        let call = IMorphoBlue::setAuthorizationWithSignatureCall {
            authorization: auth_tuple,
            signature: sig_bytes,
        };

        Ok(IMorphoBlue::setAuthorizationWithSignatureCall::abi_encode(&call).into())
    }

    /// Build the calldata for `withdrawCollateral`.
    pub fn build_withdraw_calldata(
        market_params: &MarketParams,
        assets: U256,
        on_behalf: Address,
        receiver: Address,
    ) -> Bytes {
        let mp = MarketParams {
            loanToken: market_params.loanToken,
            collateralToken: market_params.collateralToken,
            oracle: market_params.oracle,
            irm: market_params.irm,
            lltv: market_params.lltv,
        };

        let call = IMorphoBlue::withdrawCollateralCall {
            marketParams: mp,
            assets,
            onBehalf: on_behalf,
            receiver,
        };

        IMorphoBlue::withdrawCollateralCall::abi_encode(&call).into()
    }

    /// Build an atomic multicall transaction that bundles
    /// setAuthorizationWithSignature + withdrawCollateral.
    pub fn build_atomic_transaction(
        &self,
        authorization: &Authorization,
        signature: &str,
        market_params: &MarketParams,
        assets: U256,
        on_behalf: Address,
        receiver: Address,
    ) -> AppResult<TransactionRequest> {
        let auth_calldata = Self::build_auth_calldata(authorization, signature)?;
        let withdraw_calldata =
            Self::build_withdraw_calldata(market_params, assets, on_behalf, receiver);

        let multicall_call = IMulticall3::aggregate3Call {
            calls: vec![
                Call3 {
                    target: self.morpho_blue,
                    allowFailure: false,
                    callData: auth_calldata,
                },
                Call3 {
                    target: self.morpho_blue,
                    allowFailure: false,
                    callData: withdraw_calldata,
                },
            ],
        };

        let data: Bytes = IMulticall3::aggregate3Call::abi_encode(&multicall_call).into();

        let tx = TransactionRequest::default()
            .with_from(self.signer_address)
            .with_to(self.multicall3)
            .with_input(data);

        Ok(tx)
    }

    /// Execute an emergency withdrawal for an order.
    pub async fn execute_withdrawal(
        &self,
        authorization: &Authorization,
        signature: &str,
        market_params: &MarketParams,
        assets: U256,
        user_address: &str,
    ) -> AppResult<String> {
        info!("Executing withdrawal for user={}", user_address);

        // Check balance first
        let balance = self.check_balance().await?;
        if balance < self.gas_min_balance {
            return Err(AppError::Execution(format!(
                "Hot wallet balance too low: {} (min: {})",
                balance, self.gas_min_balance
            )));
        }

        let authorizer: Address = user_address.parse().map_err(|_| {
    AppError::Execution(format!("Invalid user address: {}", user_address))
})?;

        let tx = self.build_atomic_transaction(
            authorization,
            signature,
            market_params,
            assets,
            authorizer,
            authorizer,
        )?;

        // Get provider with signer — prefer Flashbots if configured
        let provider_url = self.flashbots_url.as_ref().unwrap_or(&self.rpc_url);
        let url = provider_url.parse().map_err(|e| {
            AppError::Config(format!("Invalid provider URL: {}", e))
        })?;

        // Build signer from private key
        let signer = PrivateKeySigner::from_str(&self.private_key_hex).map_err(|e| {
            AppError::Config(format!("Invalid hot wallet private key: {}", e))
        })?;

        let provider = ProviderBuilder::new()
            .wallet(signer)
            .connect_http(url);

        // Estimate gas
        let gas = provider.estimate_gas(tx.clone()).await.map_err(|e| {
            AppError::Execution(format!("Gas estimation failed: {}", e))
        })?;

        let gas_price = provider.get_gas_price().await.map_err(|e| {
            AppError::RpcError(format!("Failed to get gas price: {}", e))
        })?;

        // Build signed transaction with gas settings
        let tx = tx.with_gas_limit(gas * 120 / 100); // 20% buffer
        let tx = tx.with_gas_price(gas_price);

        // Send transaction (signed automatically by provider wallet)
        let pending_tx = provider
            .send_transaction(tx)
            .await
            .map_err(|e| AppError::Execution(format!("Transaction send failed: {}", e)))?;

        let tx_hash = format!("0x{}", hex::encode(pending_tx.tx_hash()));

        info!("Withdrawal transaction sent: {} for user {}", tx_hash, user_address);

        // Wait for confirmation
        let receipt = pending_tx
            .get_receipt()
            .await
            .map_err(|e| AppError::Execution(format!("Transaction confirmation failed: {}", e)))?;

        if receipt.status() {
            info!(
                "Withdrawal confirmed: {} (block={})",
                tx_hash,
                receipt.block_number.unwrap_or_default()
            );
            Ok(tx_hash)
        } else {
            error!("Withdrawal transaction reverted: {}", tx_hash);
            Err(AppError::Execution(format!(
                "Transaction {} reverted",
                tx_hash
            )))
        }
    }

    /// Estimate the gas cost for a withdrawal transaction.
    pub async fn estimate_gas_cost(
        &self,
        authorization: &Authorization,
        signature: &str,
        market_params: &MarketParams,
        assets: U256,
        user_address: &str,
    ) -> AppResult<U256> {
        let authorizer: Address = user_address.parse().map_err(|_| {
    AppError::Execution(format!("Invalid user address: {}", user_address))
})?;

        let tx = self.build_atomic_transaction(
            authorization,
            signature,
            market_params,
            assets,
            authorizer,
            authorizer,
        )?;

        let url = self.rpc_url.parse().map_err(|e| {
            AppError::Config(format!("Invalid RPC URL: {}", e))
        })?;
        let provider = ProviderBuilder::new().connect_http(url);

        let gas = provider.estimate_gas(tx).await.map_err(|e| {
            AppError::Execution(format!("Gas estimation failed: {}", e))
        })?;

        let gas_price = provider.get_gas_price().await.map_err(|e| {
            AppError::RpcError(format!("Failed to get gas price: {}", e))
        })?;

        Ok(U256::from(gas) * U256::from(gas_price))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_auth_calldata_encodes() {
        let auth = Authorization {
            authorizer: Address::repeat_byte(0x01),
            authorized: Address::repeat_byte(0x02),
            is_authorized: true,
            nonce: U256::from(0),
            deadline: U256::from(9999999999u64),
        };
        let sig = format!("0x{}", "ab".repeat(65));

        let calldata = BotExecutor::build_auth_calldata(&auth, &sig);
        assert!(calldata.is_ok());
        let data = calldata.unwrap();
        assert!(!data.is_empty());
        assert!(data.len() > 4);
    }

    #[test]
    fn test_build_auth_calldata_invalid_signature() {
        let auth = Authorization {
            authorizer: Address::ZERO,
            authorized: Address::ZERO,
            is_authorized: true,
            nonce: U256::ZERO,
            deadline: U256::ZERO,
        };
        let result = BotExecutor::build_auth_calldata(&auth, "not-hex");
        assert!(result.is_err());
    }

    #[test]
    fn test_build_withdraw_calldata() {
        let mp = MarketParams {
            loanToken: [0u8; 32].into(),
            collateralToken: [1u8; 32].into(),
            oracle: Address::repeat_byte(0x03),
            irm: Address::repeat_byte(0x04),
            lltv: U256::from(860000000000000000u64),
        };
        let data = BotExecutor::build_withdraw_calldata(
            &mp,
            U256::from(1000000000000000000u64),
            Address::repeat_byte(0x05),
            Address::repeat_byte(0x05),
        );
        assert!(!data.is_empty());
    }

    #[test]
    fn test_build_atomic_transaction() {
        let executor = BotExecutor::new(
            "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
            Address::repeat_byte(0x10),
            "https://eth.example.com",
            None,
            U256::from(100000000000000000u64),
        )
        .unwrap();

        let auth = Authorization {
            authorizer: Address::repeat_byte(0x01),
            authorized: executor.signer_address,
            is_authorized: true,
            nonce: U256::from(0),
            deadline: U256::from(9999999999u64),
        };
        let sig = format!("0x{}", "ab".repeat(65));

        let mp = MarketParams {
            loanToken: [0u8; 32].into(),
            collateralToken: [1u8; 32].into(),
            oracle: Address::repeat_byte(0x03),
            irm: Address::repeat_byte(0x04),
            lltv: U256::from(860000000000000000u64),
        };

        let tx = executor
            .build_atomic_transaction(
                &auth,
                &sig,
                &mp,
                U256::from(1000000000000000000u64),
                auth.authorizer,
                auth.authorizer,
            )
            .unwrap();

        assert_eq!(tx.from, Some(executor.signer_address));
        assert_eq!(tx.to, Some(alloy::primitives::TxKind::Call(executor.multicall3)));
        assert!(tx.input.input().is_some());
    }

    #[test]
    fn test_executor_default_multicall3() {
        let executor = BotExecutor::new(
            "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
            Address::ZERO,
            "https://eth.example.com",
            None,
            U256::ZERO,
        )
        .unwrap();

        let expected: Address = "0xcA11bde33A8E2b1ad6bf053c61E3A8e7e2A9d1E9"
            .parse()
            .unwrap();
        assert_eq!(executor.multicall3, expected);
    }
}
