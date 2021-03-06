use std::fs::File;
use std::io::prelude::*;

use bip39::{Mnemonic, MnemonicType, Language, Seed};
use bip32::{XPrv, ChildNumber, PrivateKeyBytes};
use bip32::secp256k1::elliptic_curve::sec1::ToEncodedPoint;
use serde::{Serialize, Deserialize};
use serde_json::Value;
use hex;
use ethereum_tx_sign::RawTransaction;

use crate::crypto::{generate_eth_address, keccak512};
use crate::{read_user_input, utils};

const RINKEBY_CHAIN_ID: u8 = 4;
const ETH_DERIVE_KEY_PATH: &str = "m/44'/60'/0'/0";

#[derive(Serialize, Deserialize)]
pub struct Wallet {
    /// Encoded wallet seed
    pub pad: Vec<u8>,
    /// The public key used to verify logins
    pub verification_key: Vec<u8>,
    /// Accounts associated with this wallet
    accounts_metadata: AccountMetadata,
}

impl Wallet {
    /// Creates a new wallet with the given password
    pub fn new(password: String) -> Wallet {
        let mnemonic = Mnemonic::new(MnemonicType::Words12, Language::English);
        let phrase = mnemonic.phrase();
        let seed = Seed::new(&mnemonic, "");
        println!("Here is your secret recovery phrase: {}", phrase);

        Wallet::generate_wallet(seed.as_bytes(), password)
    }

    /// Recreates a wallet with the given seed phrase and new password
    pub fn from(password: String, mnemonic: Mnemonic) -> Wallet {
        let seed = Seed::new(&mnemonic, "");
        Wallet::generate_wallet(seed.as_bytes(), password)
    }

    /// Utility function to generate a fresh wallet instance
    fn generate_wallet(seed: &[u8], password: String) -> Wallet {
        let pad = utils::xor(seed, &keccak512(password.as_bytes())).unwrap();
        let (_, verification_key) = utils::create_keys_from_path(seed, "m/44'/60'/0'");
        let (parent_derive_xprv, _) = utils::create_keys_from_path(seed, ETH_DERIVE_KEY_PATH);

        Wallet {
            pad,
            verification_key: verification_key.to_bytes().to_vec(),
            accounts_metadata: AccountMetadata::new(parent_derive_xprv),
        }
    }

    /// Stores the key user data that is necessary for logging in again
    pub fn store(&mut self) -> Result<(), String> {
        let mut file = File::create("userdata.txt").unwrap();

        // clear all sensitive data
        self.accounts_metadata.deriving_key = None;
        for account in &mut self.accounts_metadata.accounts {
            account.prv_key = None;
        }

        let data_bytes = serde_json::to_vec(self).unwrap();

        match file.write_all(&data_bytes) {
            Ok(()) => Ok(()),
            Err(e) => Err(format!("Error writing to file: {}", e)),
        }
    }

    pub fn verify_password(&mut self, password: String) -> bool {
        let password_hash = keccak512(password.as_bytes());
        let seed = utils::xor(&password_hash, &self.pad).unwrap();
        let (_, xpub) = utils::create_keys_from_path(&seed, "m/44'/60'/0'");

        if xpub.to_bytes().to_vec() == self.verification_key {
            // set the deriving key
            let (parent_derive_xprv, _) = utils::create_keys_from_path(&seed, ETH_DERIVE_KEY_PATH);
            self.accounts_metadata.deriving_key = Some(parent_derive_xprv);

            true
        } else {
            false
        }
    }

    /// Starts the wallet with the default account
    pub fn run(&mut self) {
        // fetch the deriving key
        let deriving_key = match &self.accounts_metadata.deriving_key {
            Some(k) => k.clone(),
            None => unreachable!("Deriving key must've been created if wallet was created"),
        };

        // start account actions
        match self.accounts_metadata.run(deriving_key) {
            5 => {
                match self.store() {
                    Ok(()) => println!("Stored wallet data safely"),
                    Err(e) => println!("{}", e),
                };
            },
            _ => unreachable!("Code should only return quit flag (5)"),
        };
    }
}

#[derive(Serialize, Deserialize)]
struct AccountMetadata {
    /// The parent private key deriving all accounts
    #[serde(skip)]
    pub deriving_key: Option<XPrv>,
    /// A vector of derived accounts
    pub accounts: Vec<Account>,
}

impl AccountMetadata {
    /// Creates AccountMetadata with the private deriving key and a default account
    pub fn new(deriving_key: XPrv) -> Self {
        AccountMetadata {
            deriving_key: Some(deriving_key.clone()),
            accounts: vec![Account::new(&deriving_key, 0)]
        }
    }

    /// Creates a new account with specified index and returns a reference to it
    pub fn create_account(&mut self, index: usize) -> &mut Account {
        match &self.deriving_key {
            Some(k) => {
                let account = Account::new(k, index);
                self.accounts.push(account);
                self.get_account(index)
            },
            None => unreachable!(),
        }
    }

    /// Returns the first account of the accounts vector
    pub fn default_account(&mut self) -> &mut Account {
        &mut self.accounts[0]
    }

    /// Prints all the created accounts in the wallet
    pub fn print_accounts(&self) {
        for (index, acc) in self.accounts.iter().enumerate() {
            println!("{}) {}", index, acc.address);
        }
    }

    /// Returns the account with given index
    pub fn get_account(&mut self, index: usize) -> &mut Account {
        &mut self.accounts[index]
    }

    /// Runs an account, allowing for creation of new accounts and switching between accounts when user opts to do so.
    pub fn run(&mut self, deriving_key: XPrv) -> u8 {
        let mut account = self.default_account();

        loop {
            match account.run(&deriving_key) {
                3 => {
                    let index = self.accounts.len();
                    account = self.create_account(index);
                },
                4 => {
                    self.print_accounts();
                    // switch to user selected account
                    let option = utils::read_user_input().parse::<usize>().unwrap();
                    account = self.get_account(option);
                },
                5 => {
                    return 5;
                },
                _ => print!("Invalid option"),
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct Account {
    /// The number of confirmed transactions sent from this account
    pub nonce: u64,
    /// The full HD derivation path of this account
    pub path: String,
    /// The address of this account
    pub address: String,
    /// The private key of the account
    prv_key: Option<PrivateKeyBytes>,
}

impl Account {
    /// Creates a new account with nonce as 0 and private_key set to none. Private key can later be
    /// instantiated when needed for signing a transaction.
    /// deriving_key - the parent key with path m/44'/60'/0'/0, used to derive all child accounts
    /// index - the index of the child account
    ///
    /// The returned key has path: m/44'/60'/0'/0/x, where x = 0,1,2,3...
    pub fn new(deriving_key: &XPrv, index: usize) -> Self {
        let child_number = ChildNumber::new(index as u32, false).unwrap();
        let child_xprv = deriving_key.derive_child(child_number).unwrap();
        let child_xpub = child_xprv.public_key();

        // Convert default acct pub_key to Ethereum address by taking hash of UNCOMPRESSED point
        let pub_key: [u8; 65] = child_xpub.public_key().to_encoded_point(false).as_bytes().try_into().unwrap();
        // only hash last 64B of pub_key because we want to leave out the prefix 0x04
        let addr_bytes = generate_eth_address(&pub_key[1..]);
        let address = String::from("0x") + &hex::encode(addr_bytes);

        let mut path = String::from("m/44'/60'/0'/0/");
        path.push_str(&index.to_string());

        Account {
            nonce: 0,
            path,
            prv_key: None,
            address,
        }
    }

    pub fn run(&mut self, deriving_key: &XPrv) -> u8 {
        println!("CURRENT ACCOUNT ADDRESS: {}", &self.address);

        loop {
            // TODO: remove manual query of account balance in place of automatic fetch
            let user_input = loop {
                println!("{}", "1) View account balance");
                println!("{}", "2) Send a transaction");
                println!("{}", "3) Create another account");
                println!("{}", "4) Switch account");
                println!("{}", "5) QUIT");

                match utils::read_user_input().parse::<u8>() {
                    Ok(option) => break option,
                    Err(_e) => {
                        println!("Invalid option");
                    }
                }
            };

            match user_input {
                1 => {
                    self.query_balance();
                },
                2 => {
                    // if prv_key is non-existent, derive it and set it. Then send transaction.
                    if let None = self.prv_key {
                        let index = self.path.split("/")
                            .into_iter()
                            .last().unwrap()
                            .parse::<u32>().unwrap();
                        self.prv_key = Some(utils::derive_child_secret_key(deriving_key, index));
                    }
                    self.send_transaction();
                },
                3 => return 3,
                4 => return 4,
                5 => return 5,
                _ => println!("{}", "Invalid option"),
            }
        }
    }

    fn query_balance(&self) {
        let resp: Value = ureq::post("https://rinkeby.infura.io/v3/39f702e71cd84987bd1ec2550a54375e")
            .set("Content-Type", "application/json")
            .send_json(ureq::json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "eth_getBalance",
                        "params": [self.address, "latest"]
                    })).unwrap()
            .into_json().unwrap();

        match resp["result"].as_str() {
            Some(s) => {
                match s.strip_prefix("0x") {
                    Some(v) => {
                        let balance = u128::from_str_radix(v, 16).unwrap();
                        println!("Balance: {} ETH", utils::wei_to_eth(balance));
                    },
                    None => println!("String doesn't start with 0x"),
                }
            },
            None => println!("Value is not a string"),
        };
    }

    fn send_transaction(&mut self) {
        let (recipient, recipient_bytes) = match utils::get_valid_address_bytes() {
            Ok(r) => (r.0, r.1),
            Err(_e) => return,
        };

        // TODO: check that amount is less than the current balance
        let eth_amount: f64 =  loop {
            println!("Enter ETH amount to send: ");
            match utils::read_user_input().parse::<f64>() {
                Ok(v) => break v,
                Err(_e) => println!("Please enter a number"),
            }
        };
        let wei_amount: u128 = utils::eth_to_wei(eth_amount);

        // estimate the gas price
        let resp: Value = ureq::post("https://rinkeby.infura.io/v3/39f702e71cd84987bd1ec2550a54375e")
            .set("Content-Type", "application/json")
            .send_json(ureq::json!({
                "jsonrpc": "2.0",
                "id": "1",
                "method": "eth_gasPrice",
                "params": []
            })).unwrap()
            .into_json().unwrap();
        let gas_price = resp["result"].as_str().unwrap().strip_prefix("0x").unwrap();
        let price = u128::from_str_radix(gas_price, 16).unwrap();

        // create and sign transaction
        let tx = RawTransaction::new(
            self.nonce as u128,
            recipient_bytes,
            wei_amount,
            price,
            21000,
            vec![]
        );
        let rlp_bytes = tx.sign(&self.prv_key.unwrap(), &RINKEBY_CHAIN_ID);
        let mut final_txn = String::from("0x");
        final_txn.push_str(&hex::encode(rlp_bytes));

        println!("Transaction details:\n\tTO: {:?}\n\tAMOUNT: {} ETH\n\tGAS PRICE: {} wei\n\t", recipient, eth_amount, price);
        println!("Press 1 to CONFIRM");
        println!("Press any other number to CANCEL");
        let user_option = loop {
            match read_user_input().parse::<u8>() {
                Ok(v) => break v,
                Err(_e) => println!("Please enter a number"),
            }
        };

        match user_option {
            1 => {
                let resp: Value = ureq::post("https://rinkeby.infura.io/v3/39f702e71cd84987bd1ec2550a54375e")
                    .set("Content-Type", "application/json")
                    .send_json(ureq::json!({
                        "jsonrpc": "2.0",
                        "id": "1",
                        "method": "eth_sendRawTransaction",
                        "params": [final_txn],
                    })).unwrap()
                    .into_json().unwrap();

                if let Some(s) = resp["result"].as_str() {
                    if s != "0x0" {
                        self.nonce += 1;
                        println!("Transaction {} successfully sent", s);
                    } else {
                        println!("Transaction not yet available");
                    }
                } else {
                    println!("Error occurred in sending transaction");
                }
            },
            _ => println!("Transaction canceled")
        };
    }
}