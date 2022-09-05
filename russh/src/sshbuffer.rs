// Copyright 2016 Pierre-Étienne Meunier
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

use std::num::Wrapping;

use super::*;

/// The SSH id buffer.
#[derive(Debug)]
pub enum SSHId {
    /// When sending the id, append RFC standard '\r\n'
    Standard(String),
    /// When sending the id, use this buffer as it is and do not append additional line terminators.
    Raw(String),
}

impl SSHId {
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::Standard(s) => format!("{}\r\n", s).as_bytes().to_vec(),
            Self::Raw(s) => s.as_bytes().to_vec(),
        }
    }
}

#[derive(Debug, Default)]
pub struct SSHBuffer {
    pub buffer: CryptoVec,
    pub len: usize, // next packet length.
    pub bytes: usize,
    // Sequence numbers are on 32 bits and wrap.
    // https://tools.ietf.org/html/rfc4253#section-6.4
    pub seqn: Wrapping<u32>,
}

impl SSHBuffer {
    pub fn new() -> Self {
        SSHBuffer {
            buffer: CryptoVec::new(),
            len: 0,
            bytes: 0,
            seqn: Wrapping(0),
        }
    }

    pub fn send_ssh_id(&mut self, id: &SSHId) {
        self.buffer.extend(&id.to_bytes());
    }
}
