import { useOutletContext } from 'react-router-dom';
import type { AgentDetail } from '../api/types';

interface AgentContext {
  agent: AgentDetail | null;
  agentId: string;
}

export function useAgentContext(): AgentContext {
  return useOutletContext<AgentContext>();
}
