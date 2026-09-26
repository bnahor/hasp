"""Harbor binding (Terminal-Bench): `harbor run -a harbor_agent:HarnessAgent`."""

from harbor.agents.base import BaseAgent

from harness_fh import HarnessAgentMixin


class HarnessAgent(HarnessAgentMixin, BaseAgent):
    pass
