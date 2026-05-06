use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WalletError {
    #[error("input cannot be empty")]
    EmptyInput,
}

pub trait KeyProvider {
    fn public_id(&self) -> String;
    fn sign_psbt(&self, psbt_base64: &str) -> Result<String, WalletError>;
}

pub trait WalletBackend {
    fn create_rgb_invoice(&self, contract_id: &str, amount: u64) -> Result<String, WalletError>;

    fn sign_psbt(&self, psbt_base64: &str) -> Result<String, WalletError>;

    fn import_consignment(&self, consignment_base64: &str) -> Result<(), WalletError>;
}

#[derive(Debug, Clone)]
pub struct MockKeyProvider {
    public_id: String,
}

impl MockKeyProvider {
    pub fn new(public_id: impl Into<String>) -> Self {
        Self {
            public_id: public_id.into(),
        }
    }
}

impl Default for MockKeyProvider {
    fn default() -> Self {
        Self::new("mock-key")
    }
}

impl KeyProvider for MockKeyProvider {
    fn public_id(&self) -> String {
        self.public_id.clone()
    }

    fn sign_psbt(&self, psbt_base64: &str) -> Result<String, WalletError> {
        if psbt_base64.is_empty() {
            return Err(WalletError::EmptyInput);
        }

        Ok(format!("mock-signed-psbt:{}:{psbt_base64}", self.public_id))
    }
}

#[derive(Debug, Clone)]
pub struct MockWalletBackend<K = MockKeyProvider> {
    key_provider: K,
}

impl MockWalletBackend {
    pub fn new_default() -> Self {
        Self {
            key_provider: MockKeyProvider::default(),
        }
    }
}

impl<K> MockWalletBackend<K> {
    pub fn new(key_provider: K) -> Self {
        Self { key_provider }
    }
}

impl Default for MockWalletBackend {
    fn default() -> Self {
        Self::new_default()
    }
}

impl<K> WalletBackend for MockWalletBackend<K>
where
    K: KeyProvider,
{
    fn create_rgb_invoice(&self, contract_id: &str, amount: u64) -> Result<String, WalletError> {
        if contract_id.is_empty() {
            return Err(WalletError::EmptyInput);
        }

        Ok(format!(
            "rgb:{}:{}:{}",
            contract_id,
            amount,
            self.key_provider.public_id()
        ))
    }

    fn sign_psbt(&self, psbt_base64: &str) -> Result<String, WalletError> {
        self.key_provider.sign_psbt(psbt_base64)
    }

    fn import_consignment(&self, consignment_base64: &str) -> Result<(), WalletError> {
        if consignment_base64.is_empty() {
            return Err(WalletError::EmptyInput);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_wallet_creates_rgb_invoice() {
        let wallet = MockWalletBackend::default();

        let invoice = wallet.create_rgb_invoice("contract-1", 42).unwrap();

        assert_eq!(invoice, "rgb:contract-1:42:mock-key");
    }

    #[test]
    fn mock_wallet_signs_psbt_deterministically() {
        let wallet = MockWalletBackend::default();

        let signed = wallet.sign_psbt("cHNidA==").unwrap();

        assert_eq!(signed, "mock-signed-psbt:mock-key:cHNidA==");
    }

    #[test]
    fn mock_wallet_imports_non_empty_consignment() {
        let wallet = MockWalletBackend::default();

        assert_eq!(wallet.import_consignment("Y29uc2lnbm1lbnQ="), Ok(()));
    }

    #[test]
    fn mock_wallet_rejects_empty_consignment() {
        let wallet = MockWalletBackend::default();

        assert_eq!(wallet.import_consignment(""), Err(WalletError::EmptyInput));
    }
}
