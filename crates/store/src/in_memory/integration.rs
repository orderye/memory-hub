use super::*;

#[async_trait]
impl mem_core::IntegrationStore for InMemoryHubStore {
    async fn save_proposal(&self, p: &IntegrationProposal) -> Result<()> {
        self.proposals.write().unwrap().insert(p.id, p.clone());
        Ok(())
    }

    async fn get_proposal(&self, id: Uuid) -> Result<IntegrationProposal> {
        self.proposals
            .read()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(HubError::NotFound)
    }

    async fn list_proposals(
        &self,
        namespace: &str,
        status: Option<ProposalStatus>,
    ) -> Result<Vec<IntegrationProposal>> {
        let mut out: Vec<IntegrationProposal> = self
            .proposals
            .read()
            .unwrap()
            .values()
            .filter(|p| p.namespace == namespace)
            .filter(|p| status.is_none_or(|s| p.status == s))
            .cloned()
            .collect();
        out.sort_by_key(|b| std::cmp::Reverse(b.created_at));
        Ok(out)
    }

    async fn open_exists(&self, namespace: &str, members: &[(String, Vec<Uuid>)]) -> Result<bool> {
        let guard = self.proposals.read().unwrap();
        Ok(guard.values().any(|p| {
            p.status == ProposalStatus::Open
                && p.namespace == namespace
                && members.iter().any(|(kind, ids)| {
                    if kind != p.kind.as_str() || ids.len() != p.member_ids.len() {
                        return false;
                    }
                    let s1: std::collections::HashSet<_> = ids.iter().collect();
                    let s2: std::collections::HashSet<_> = p.member_ids.iter().collect();
                    s1 == s2
                })
        }))
    }
}

#[async_trait]
impl mem_core::CognitiveProposalStore for InMemoryHubStore {
    async fn save_cognitive_proposal(&self, p: &CognitiveProposalRecord) -> Result<()> {
        let mut w = self.cognitive_proposals.write().unwrap();
        if w.contains_key(&p.id) {
            return Err(HubError::Conflict);
        }
        w.insert(p.id, p.clone());
        Ok(())
    }

    async fn get_cognitive_proposal(&self, id: Uuid) -> Result<CognitiveProposalRecord> {
        self.cognitive_proposals
            .read()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or(HubError::NotFound)
    }

    async fn list_cognitive_proposals(
        &self,
        namespace: &str,
        status: Option<CognitiveProposalStatus>,
    ) -> Result<Vec<CognitiveProposalRecord>> {
        let mut out: Vec<CognitiveProposalRecord> = self
            .cognitive_proposals
            .read()
            .unwrap()
            .values()
            .filter(|p| p.namespace == namespace)
            .filter(|p| status.is_none_or(|s| p.status == s))
            .cloned()
            .collect();
        out.sort_by_key(|p| std::cmp::Reverse(p.created_at));
        Ok(out)
    }

    async fn update_cognitive_proposal_status(
        &self,
        id: Uuid,
        expected_status: CognitiveProposalStatus,
        new_status: CognitiveProposalStatus,
        rejection_reason: Option<&str>,
        resulting_memory_id: Option<Uuid>,
        resolved_by: Option<&str>,
    ) -> Result<CognitiveProposalRecord> {
        let mut w = self.cognitive_proposals.write().unwrap();
        let p = w.get_mut(&id).ok_or(HubError::NotFound)?;
        if p.status != expected_status {
            return Err(HubError::Conflict);
        }
        p.status = new_status;
        p.rejection_reason = rejection_reason.map(str::to_string);
        p.resulting_memory_id = resulting_memory_id;
        p.resolved_by = resolved_by.map(str::to_string);
        p.updated_at = Utc::now();
        Ok(p.clone())
    }
}
