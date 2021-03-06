use super::config;
use crate::encoder;
use crate::errors::{Error, ErrorKind, Result};
use crate::protos::{xchain, xendorser};
use num_bigint;
use num_traits;
use num_traits::cast::FromPrimitive;
use serde_json;
use std::ops::AddAssign;
use std::ops::Sub;
use std::prelude::v1::*;
use std::slice;
extern crate sgx_types;
use sgx_types::*;

#[derive(Default)]
pub struct Message {
    pub to: String,
    pub amount: String,
    pub fee: String,
    pub desc: String,
    pub frozen_height: i64,
    pub initiator: String,
    pub auth_require: Vec<String>,
}

pub struct Session<'a, 'b, 'c> {
    pub chain_name: &'a String,

    account: &'b super::wallet::Account,

    msg: &'c Message,
}

impl<'a, 'b, 'c> Session<'a, 'b, 'c> {
    pub fn new(c: &'a String, w: &'b super::wallet::Account, m: &'c Message) -> Self {
        Session {
            msg: m,
            chain_name: c,
            account: w,
        }
    }

    pub fn check_resp_code(&self, resp: &[xchain::ContractResponse]) -> Result<()> {
        for i in resp.iter() {
            if i.status > 400 {
                return Err(Error::from(ErrorKind::ContractCodeGT400));
            }
        }
        Ok(())
    }

    pub fn pre_exec_with_select_utxo(
        &self,
        pre_sel_utxo_req: xchain::PreExecWithSelectUTXORequest,
    ) -> Result<xchain::PreExecWithSelectUTXOResponse> {
        let request_data = serde_json::to_string(&pre_sel_utxo_req)?;
        let mut endorser_request = xendorser::EndorserRequest::new();
        endorser_request.set_RequestName(String::from("PreExecWithFee"));
        endorser_request.set_BcName(self.chain_name.to_owned());
        endorser_request.set_RequestData(request_data.into_bytes());

        let mut rt: sgx_status_t = sgx_status_t::SGX_ERROR_UNEXPECTED;
        let req = serde_json::to_string(&endorser_request)?;

        let mut output = 0 as *mut sgx_libc::c_void;
        let mut out_len: usize = 0;
        let resp = unsafe {
            crate::ocall_xchain_endorser_call(
                &mut rt,
                req.as_ptr() as *const u8,
                req.len(),
                &mut output,
                &mut out_len,
            )
        };

        if resp != sgx_status_t::SGX_SUCCESS || rt != sgx_status_t::SGX_SUCCESS {
            println!(
                "[-] pre_exec_with_select_utxo ocall_xchain_endorser_call failed: {}, {}!",
                resp.as_str(),
                rt.as_str()
            );
            return Err(Error::from(ErrorKind::InvalidArguments));
        }
        unsafe {
            if sgx_types::sgx_is_outside_enclave(output, out_len) == 0 {
                println!("[-] alloc error");
                return Err(Error::from(ErrorKind::InvalidArguments));
            }
        }

        let resp_slice = unsafe { slice::from_raw_parts(output as *mut u8, out_len) };
        let endorser_resp: xendorser::EndorserResponse = serde_json::from_slice(&resp_slice)?;
        let pre_exec_with_select_utxo_resp: xchain::PreExecWithSelectUTXOResponse =
            serde_json::from_slice(&endorser_resp.ResponseData)?;
        self.check_resp_code(
            pre_exec_with_select_utxo_resp
                .get_response()
                .get_responses(),
        )?;
        unsafe {
            crate::ocall_free(output);
        }
        Ok(pre_exec_with_select_utxo_resp)
    }

    fn generate_tx_input(
        &self,
        utxo_output: &xchain::UtxoOutput,
        total_need: &num_bigint::BigInt,
    ) -> Result<(Vec<xchain::TxInput>, xchain::TxOutput)> {
        let mut tx_inputs = std::vec::Vec::<xchain::TxInput>::new();
        for utxo in utxo_output.utxoList.iter() {
            let mut ti = xchain::TxInput::new();
            ti.set_ref_txid(utxo.refTxid.clone());
            ti.set_ref_offset(utxo.refOffset);
            ti.set_from_addr(utxo.toAddr.clone());
            ti.set_amount(utxo.amount.clone());
            tx_inputs.push(ti);
        }

        let utxo_total = crate::consts::str_as_bigint(&utxo_output.totalSelected)?;

        let mut to = xchain::TxOutput::new();
        if utxo_total.cmp(total_need) == std::cmp::Ordering::Greater {
            let delta = utxo_total.sub(total_need);
            to.set_to_addr(self.account.address.clone().into_bytes());
            to.set_amount(delta.to_bytes_be().1);
        }
        return Ok((tx_inputs, to));
    }

    fn generate_tx_output(
        &self,
        to: &String,
        amount: &String,
        fee: &str,
    ) -> Result<Vec<xchain::TxOutput>> {
        let mut tx_outputs = std::vec::Vec::<xchain::TxOutput>::new();
        //TODO amount > 0
        if !to.is_empty() {
            let mut t = xchain::TxOutput::new();
            t.set_to_addr(to.clone().into_bytes());
            let am = crate::consts::str_as_bigint(&amount)?;
            t.set_amount(am.to_bytes_be().1);
            tx_outputs.push(t);
        }
        if !fee.is_empty() && fee != "0" {
            let mut t = xchain::TxOutput::new();
            t.set_to_addr(String::from("$").into_bytes());
            let am = crate::consts::str_as_bigint(&fee)?;
            t.set_amount(am.to_bytes_be().1);
            tx_outputs.push(t);
        }
        Ok(tx_outputs)
    }

    pub fn gen_compliance_check_tx(
        &self,
        resp: &mut xchain::PreExecWithSelectUTXOResponse,
    ) -> Result<xchain::Transaction> {
        let total_need = num_bigint::BigInt::from_i64(
            config::CONFIG
                .read()
                .unwrap()
                .compliance_check
                .compliance_check_endorse_service_fee as i64,
        )
        .ok_or(Error::from(ErrorKind::ParseError))?;

        let (tx_inputs, tx_output) = self.generate_tx_input(resp.get_utxoOutput(), &total_need)?;
        let mut tx_outputs = self.generate_tx_output(
            &config::CONFIG
                .read()
                .unwrap()
                .compliance_check
                .compliance_check_endorse_service_fee_addr,
            &config::CONFIG
                .read()
                .unwrap()
                .compliance_check
                .compliance_check_endorse_service_fee
                .to_string(),
            "0",
        )?;

        if !tx_output.to_addr.is_empty() {
            tx_outputs.push(tx_output);
        }

        // compose transaction
        let mut tx = xchain::Transaction::new();
        tx.set_desc(String::from("compliance check tx").into_bytes());
        tx.set_version(super::consts::TXVersion);
        tx.set_coinbase(false);
        tx.set_timestamp(super::consts::now_as_nanos());
        tx.set_tx_inputs(protobuf::RepeatedField::from_vec(tx_inputs));
        tx.set_tx_outputs(protobuf::RepeatedField::from_vec(tx_outputs));
        tx.set_initiator(self.msg.initiator.to_owned());
        tx.set_nonce(super::wallet::get_nonce()?);

        let digest_hash = encoder::make_tx_digest_hash(&tx)?;

        //sign the digest_hash
        let sig = self.account.sign(&digest_hash)?;
        let mut signature_info = xchain::SignatureInfo::new();
        signature_info.set_PublicKey(self.account.public_key()?);
        signature_info.set_Sign(sig);
        let signature_infos = vec![signature_info; 1];
        tx.set_initiator_signs(protobuf::RepeatedField::from_vec(signature_infos));
        tx.set_txid(encoder::make_transaction_id(&tx)?);
        Ok(tx)
    }

    pub fn gen_real_tx(
        &self,
        resp: &xchain::PreExecWithSelectUTXOResponse,
        cctx: &xchain::Transaction,
    ) -> Result<xchain::Transaction> {
        let mut tx_outputs =
            self.generate_tx_output(&self.msg.to, &self.msg.amount, &self.msg.fee)?;

        let mut total_selected: num_bigint::BigInt = num_traits::Zero::zero();
        let mut utxo_list = std::vec::Vec::<xchain::Utxo>::new();
        let mut index = 0;
        for tx_output in cctx.tx_outputs.iter() {
            if tx_output.to_addr == self.msg.initiator.as_bytes() {
                let mut t = xchain::Utxo::new();
                t.set_amount(tx_output.amount.clone());
                t.set_toAddr(tx_output.to_addr.clone());
                t.set_refTxid(cctx.txid.clone());
                t.set_refOffset(index);
                utxo_list.push(t);
                let um = num_bigint::BigInt::from_bytes_be(
                    num_bigint::Sign::Plus,
                    &tx_output.amount[..],
                );
                total_selected.add_assign(um);
            };
            index += 1;
        }
        let mut utxo_output = xchain::UtxoOutput::new();
        utxo_output.set_utxoList(protobuf::RepeatedField::from_vec(utxo_list));
        utxo_output.set_totalSelected(total_selected.to_str_radix(10));

        let mut total_need = crate::consts::str_as_bigint(&self.msg.amount)?;
        let fee = crate::consts::str_as_bigint(&self.msg.fee)?;
        total_need.add_assign(fee);

        let (tx_inputs, delta_tx_ouput) = self.generate_tx_input(&utxo_output, &total_need)?;
        if !delta_tx_ouput.to_addr.is_empty() {
            tx_outputs.push(delta_tx_ouput);
        }
        let mut tx = xchain::Transaction::new();
        tx.set_desc(vec![]);
        tx.set_version(super::consts::TXVersion);
        tx.set_coinbase(false);
        tx.set_timestamp(super::consts::now_as_nanos());
        tx.set_tx_inputs(protobuf::RepeatedField::from_vec(tx_inputs));
        tx.set_tx_outputs(protobuf::RepeatedField::from_vec(tx_outputs));
        tx.set_initiator(self.msg.initiator.to_owned());
        tx.set_nonce(super::wallet::get_nonce()?);
        tx.set_auth_require(protobuf::RepeatedField::from_vec(
            self.msg.auth_require.to_owned(),
        ));

        tx.set_tx_inputs_ext(resp.get_response().inputs.clone());
        tx.set_tx_outputs_ext(resp.get_response().outputs.clone());
        tx.set_contract_requests(resp.get_response().requests.clone());

        let digest_hash = encoder::make_tx_digest_hash(&tx)?;

        //sign the digest_hash
        let sig = self.account.sign(&digest_hash)?;
        let mut signature_info = xchain::SignatureInfo::new();

        signature_info.set_PublicKey(self.account.public_key()?);
        signature_info.set_Sign(sig);
        let signature_infos = vec![signature_info; 1];
        tx.set_initiator_signs(protobuf::RepeatedField::from_vec(signature_infos.clone()));
        if !self.account.contract_name.is_empty() {
            tx.set_auth_require_signs(protobuf::RepeatedField::from_vec(signature_infos));
        }

        tx.set_txid(encoder::make_transaction_id(&tx)?);
        Ok(tx)
    }

    pub fn compliance_check(
        &self,
        tx: &xchain::Transaction,
        fee: &xchain::Transaction,
    ) -> Result<xchain::SignatureInfo> {
        let mut tx_status = xchain::TxStatus::new();
        tx_status.set_bcname(self.chain_name.to_owned());
        tx_status.set_tx(tx.clone());
        let request_data = serde_json::to_string(&tx_status)?;
        let mut endorser_request = xendorser::EndorserRequest::new();
        endorser_request.set_RequestName(String::from("ComplianceCheck"));
        endorser_request.set_BcName(self.chain_name.to_owned());
        endorser_request.set_Fee(fee.clone());
        endorser_request.set_RequestData(request_data.into_bytes());

        let mut rt: sgx_status_t = sgx_status_t::SGX_ERROR_UNEXPECTED;
        let req = serde_json::to_string(&endorser_request)?;

        let mut output = 0 as *mut sgx_libc::c_void;
        let mut out_len: usize = 0;
        let resp = unsafe {
            crate::ocall_xchain_endorser_call(
                &mut rt,
                req.as_ptr() as *const u8,
                req.len(),
                &mut output,
                &mut out_len,
            )
        };

        if resp != sgx_status_t::SGX_SUCCESS || rt != sgx_status_t::SGX_SUCCESS {
            println!(
                "[-] compliance_check ocall_xchain_endorser_call failed: {}, {}!",
                resp.as_str(),
                rt.as_str()
            );
            return Err(Error::from(ErrorKind::InvalidArguments));
        }
        unsafe {
            if sgx_types::sgx_is_outside_enclave(output, out_len) == 0 {
                println!("[-] alloc error");
                return Err(Error::from(ErrorKind::InvalidArguments));
            }
        }

        let resp_slice = unsafe { slice::from_raw_parts(output as *mut u8, out_len) };
        let result: xendorser::EndorserResponse = serde_json::from_slice(resp_slice).unwrap();
        unsafe {
            crate::ocall_free(output);
        }
        Ok(result.EndorserSign.unwrap())
    }

    pub fn gen_complete_tx_and_post(
        &self,
        pre_exec_resp: &mut xchain::PreExecWithSelectUTXOResponse,
    ) -> Result<String> {
        let cctx = self.gen_compliance_check_tx(pre_exec_resp)?;
        let mut tx = self.gen_real_tx(&pre_exec_resp, &cctx)?;
        let end_sign = self.compliance_check(&tx, &cctx)?;

        tx.auth_require_signs.push(end_sign);
        tx.set_txid(encoder::make_transaction_id(&tx)?);

        let mut rt: sgx_status_t = sgx_status_t::SGX_ERROR_UNEXPECTED;
        let tran = serde_json::to_string(&tx)?;

        let resp =
            unsafe { crate::ocall_xchain_post_tx(&mut rt, tran.as_ptr() as *const u8, tran.len()) };

        if resp != sgx_status_t::SGX_SUCCESS || rt != sgx_status_t::SGX_SUCCESS {
            println!(
                "[-] ocall_xchain_post_tx failed: {}, {}!",
                resp.as_str(),
                rt.as_str()
            );
            return Err(Error::from(ErrorKind::InvalidArguments));
        }
        Ok(hex::encode(tx.txid))
    }

    #[allow(dead_code)]
    fn print_tx(&self, tx: &xchain::Transaction) {
        for i in tx.tx_inputs.iter() {
            crate::consts::print_bytes_num(&i.amount);
        }
        for i in tx.tx_outputs.iter() {
            crate::consts::print_bytes_num(&i.amount);
        }
    }

    //TODO
    //pub fn get_balance() -> Result<String> {}
}
