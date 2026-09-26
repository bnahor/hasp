"""Pier binding (DeepSWE): `pier run --agent-import-path pier_agent:HarnessAgent`."""

from pier.agents.base import BaseAgent
from pier.models.agent.network import NetworkAllowlist

from harness_fh import HarnessAgentMixin, route


class HarnessAgent(HarnessAgentMixin, BaseAgent):
    def network_allowlist(self) -> NetworkAllowlist:
        # Only the model provider: Pier's proxy blocks everything else.
        return NetworkAllowlist(domains=[route(self.model_name)[3]])
