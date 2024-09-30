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

use eyre::Result;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;

use crate::contract::{EventWithMetadata, ScribeOptimisticProvider};

pub struct ContractHandler<C>
where
    C: ScribeOptimisticProvider,
{
    pub contract: C,
    cancel: CancellationToken,
    rx: Receiver<EventWithMetadata>,
}

impl<C: ScribeOptimisticProvider> ContractHandler<C> {
    pub fn new(cancel: CancellationToken, contract: C, rx: Receiver<EventWithMetadata>) -> Self {
        Self {
            cancel,
            contract,
            rx,
        }
    }

    pub async fn start(&mut self) -> Result<()> {
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    log::info!("[{:?}] Cancellation requested, stopping contract handler", self.contract.address());
                    break;
                }
                event = self.rx.recv() => {
                    log::debug!("[{:?}] Received event: {:?}", self.contract.address(), event);
                    // TODO: Handle the event
                }
            }
        }
        todo!()
    }
}
