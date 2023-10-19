//  Copyright (C) 2021-2023 Chronicle Labs, Inc.
//
//  This program is free software: you can redistribute it and/or modify
//  it under the terms of the GNU Affero General Public License as
//  published by the Free Software Foundation, either version 3 of the
//  License, or (at your option) any later version.
//
//  This program is distributed in the hope that it will be useful,
//  but WITHOUT ANY WARRANTY; without even the implied warranty of
//  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
//  GNU Affero General Public License for more details.
//
//  You should have received a copy of the GNU Affero General Public License
//  along with this program.  If not, see <http://www.gnu.org/licenses/>.

use ethers::signers::LocalWallet;
use eyre::{bail, Result, WrapErr};
use log::debug;
use std::{
    fs,
    path::{Path, PathBuf},
};

pub trait PrivateKeyWallet {
    fn private_key(&self) -> Option<String>;

    #[track_caller]
    fn get_from_private_key(&self, private_key: &str) -> Result<Option<LocalWallet>> {
        debug!("Using private key from arguments");

        let privk = private_key.trim().strip_prefix("0x").unwrap_or(private_key);
        match privk.parse::<LocalWallet>() {
            Ok(wallet) => Ok(Some(wallet)),
            Err(err) => bail!("Failed to parse private key: {:?}", err),
        }
    }
}

pub trait KeystoreWallet {
    fn keystore(&self) -> Option<String>;

    fn password(&self) -> Option<String>;

    fn password_file(&self) -> Option<String>;

    /// Ensures the path to the keystore exists.
    ///
    /// if the path is a directory, it bails and asks the user to specify the keystore file
    /// directly.
    fn find_keystore_file(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let path = path.as_ref();
        if !path.exists() {
            bail!("Keystore file `{path:?}` does not exist")
        }

        if path.is_dir() {
            bail!("Keystore path `{path:?}` is a directory. Please specify the keystore file directly.")
        }

        Ok(path.to_path_buf())
    }

    /// Attempts to read the keystore password from the password file.
    fn password_from_file(&self, password_file: impl AsRef<Path>) -> Result<String> {
        let password_file = password_file.as_ref();
        if !password_file.is_file() {
            bail!("Keystore password file `{password_file:?}` does not exist")
        }

        Ok(fs::read_to_string(password_file)?.trim_end().to_string())
    }

    fn get_from_keystore(
        &self,
        keystore_path: Option<&String>,
        keystore_password: Option<&String>,
        keystore_password_file: Option<&String>,
    ) -> Result<Option<LocalWallet>> {
        Ok(
            match (keystore_path, keystore_password, keystore_password_file) {
                (Some(path), Some(password), _) => {
                    let path = self.find_keystore_file(path)?;

                    debug!("Using Keystore file from `{path:?}` and raw password");

                    Some(
                        LocalWallet::decrypt_keystore(&path, password)
                            .wrap_err_with(|| format!("Failed to decrypt keystore {path:?}"))?,
                    )
                }
                (Some(path), _, Some(password_file)) => {
                    let path = self.find_keystore_file(path)?;

                    debug!("Using Keystore file from `{path:?}` and password from file");

                    Some(
                        LocalWallet::decrypt_keystore(&path, self.password_from_file(password_file)?)
                            .wrap_err_with(|| format!("Failed to decrypt keystore {path:?} with password file {password_file:?}"))?,
                    )
                }
                (Some(path), None, None) => {
                    let path = self.find_keystore_file(path)?;

                    debug!(
                        "Using Keystore file from `{path:?}` and password from interactive prompt"
                    );

                    let password = rpassword::prompt_password("Enter keystore password:")?;
                    Some(LocalWallet::decrypt_keystore(path, password)?)
                }
                (None, _, _) => None,
            },
        )
    }
}

pub trait CustomWallet: PrivateKeyWallet + KeystoreWallet {
    /// Generates wallet for future sign
    fn wallet(&self) -> Result<Option<LocalWallet>> {
        match (&self.private_key(), &self.keystore()) {
            (Some(secret), _) => self.get_from_private_key(secret),
            (_, Some(key)) => self.get_from_keystore(
                Some(key),
                self.password().as_ref(),
                self.password_file().as_ref(),
            ),
            (_, _) => bail!("Please provide private key or keystore"),
        }
    }
}
