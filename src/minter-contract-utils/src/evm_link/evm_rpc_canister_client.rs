mod did;

use std::future::Future;
use std::pin::Pin;

use candid::Principal;
use jsonrpc_core::{Request, Response};
use num_traits::ToPrimitive;

pub use self::did::RpcService;
use self::did::{RequestCostResult, RequestResult, Service};

#[derive(Debug, Clone)]
pub struct EvmRpcCanisterClient {
    principal: Principal,
    rpc_service: RpcService,
}

impl EvmRpcCanisterClient {
    pub fn new(principal: Principal, rpc_service: RpcService) -> Self {
        Self {
            principal,
            rpc_service,
        }
    }

    pub fn send_rpc_request(
        &self,
        request: Request,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Response>> + Send>> {
        let rpc_service = self.rpc_service.clone();
        Box::pin(Self::__send_rpc_request(
            self.principal,
            rpc_service,
            request,
        ))
    }

    async fn __send_rpc_request(
        principal: Principal,
        rpc_service: RpcService,
        request: Request,
    ) -> anyhow::Result<Response> {
        let service = Service(principal);
        const MAX_RESPONSE_SIZE: u64 = 1024 * 10;
        let request = serde_json::to_string(&request)?;

        // get request cost as cycles
        let (request_cost_result,) = service
            .request_cost(&rpc_service, &request, MAX_RESPONSE_SIZE)
            .await
            .map_err(|(err, msg)| anyhow::anyhow!("request_cost failed: {err:?}, msg: {msg}",))?;

        let cycles = match request_cost_result {
            RequestCostResult::Ok(cycles) => cycles
                .0
                .to_u128()
                .ok_or_else(|| anyhow::anyhow!("cycles conversion failed"))?,
            RequestCostResult::Err(err) => {
                anyhow::bail!("request_cost error: {err}");
            }
        };

        // send rpc request
        let (request_result,) = service
            .request(&rpc_service, &request, MAX_RESPONSE_SIZE, cycles)
            .await
            .map_err(|(err, msg)| anyhow::anyhow!("request failed: {err:?}, msg: {msg}",))?;

        let response = match request_result {
            RequestResult::Ok(response) => serde_json::from_str(&response)?,
            RequestResult::Err(err) => {
                anyhow::bail!("request error: {err}");
            }
        };

        Ok(response)
    }
}
