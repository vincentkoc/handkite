use super::super::{
    EndProvisioningData, ProvisioningDataError, ProvisioningDataProvider, StartProvisioningData,
};
use super::{
    AppleProvisioningContext, ApprovedEndpoints, LookupCandidates, MAX_LOOKUP_BYTES,
    MAX_RESPONSE_BYTES, ReviewedEndpointPolicy, client::AppleClient, encode_finish_request,
    encode_start_request, parse_finish_response, parse_start_response,
};
use std::future::Future;

const LOOKUP_URL: &str = "https://gsa.apple.com/grandslam/GsService2/lookup";

#[derive(PartialEq, Eq)]
enum Stage {
    Fresh,
    Started,
    Consumed,
}

/// One start/finish exchange. Failure or cancellation permanently consumes its stage.
pub struct AppleProvisioningProvider {
    client: AppleClient,
    endpoints: ApprovedEndpoints,
    context: AppleProvisioningContext,
    stage: Stage,
}

impl AppleProvisioningProvider {
    /// Reject an empty policy before any lookup, DNS resolution or WSS connection.
    pub async fn discover(context: AppleProvisioningContext) -> Result<Self, &'static str> {
        let policy = ReviewedEndpointPolicy::production();
        require_reviewed_policy(&policy)?;
        let client = AppleClient::new()?;
        Self::discover_with(client, policy, context).await
    }

    async fn discover_with(
        client: AppleClient,
        policy: ReviewedEndpointPolicy,
        context: AppleProvisioningContext,
    ) -> Result<Self, &'static str> {
        require_reviewed_policy(&policy)?;
        let response = client
            .lookup(
                reqwest::Url::parse(LOOKUP_URL).expect("constant HTTPS URL"),
                &context,
                MAX_LOOKUP_BYTES,
            )
            .await?;
        let candidates = LookupCandidates::parse(200, &response.body)?;
        let endpoints = policy.approve(&candidates)?;
        Ok(Self {
            client,
            endpoints,
            context,
            stage: Stage::Fresh,
        })
    }

    pub fn identifier(&self) -> &super::ProvisioningIdentifier {
        self.context.identifier()
    }
}

fn require_reviewed_policy(policy: &ReviewedEndpointPolicy) -> Result<(), &'static str> {
    if policy.pairs.is_empty() {
        return Err("Apple provisioning destinations are not approved");
    }
    Ok(())
}

impl ProvisioningDataProvider for AppleProvisioningProvider {
    fn start(
        &mut self,
    ) -> impl Future<Output = Result<StartProvisioningData, ProvisioningDataError>> {
        let permitted = self.stage == Stage::Fresh;
        self.stage = Stage::Consumed;
        async move {
            if !permitted {
                return Err(ProvisioningDataError);
            }
            let response = self
                .client
                .post(
                    self.endpoints.start.clone(),
                    &self.context,
                    encode_start_request(),
                    MAX_RESPONSE_BYTES,
                )
                .await
                .map_err(|_| ProvisioningDataError)?;
            let data =
                parse_start_response(200, &response.body).map_err(|_| ProvisioningDataError)?;
            self.stage = Stage::Started;
            Ok(data)
        }
    }

    fn finish(
        &mut self,
        cpim: &str,
    ) -> impl Future<Output = Result<EndProvisioningData, ProvisioningDataError>> {
        let permitted = self.stage == Stage::Started;
        self.stage = Stage::Consumed;
        async move {
            if !permitted {
                return Err(ProvisioningDataError);
            }
            let body = encode_finish_request(cpim).map_err(|_| ProvisioningDataError)?;
            let response = self
                .client
                .post(
                    self.endpoints.finish.clone(),
                    &self.context,
                    body,
                    MAX_RESPONSE_BYTES,
                )
                .await
                .map_err(|_| ProvisioningDataError)?;
            parse_finish_response(200, &response.body).map_err(|_| ProvisioningDataError)
        }
    }
}

#[cfg(test)]
pub(super) mod tests;
