import { useTranslation } from 'react-i18next';
import { useAgentContext } from '../hooks/useAgentContext';
import { DiscordBotSection } from './agent-overview/DiscordBotSection';
import { LlmModelSection } from './agent-overview/LlmModelSection';
import { McpSection } from './agent-overview/McpSection';
import { NostrSection } from './agent-overview/NostrSection';
import { NostrRelaySection } from './agent-overview/NostrRelaySection';
import { ActionCard, DetailRow } from './agent-overview/presentation';

export { LlmModelSection, NostrRelaySection };

export default function AgentOverview() {
  const { t } = useTranslation();
  const { agent, agentId } = useAgentContext();

  if (!agent) return null;

  return (
    <>
      {/* Action cards */}
      <div className="grid grid-cols-1 md:grid-cols-3 gap-4 mb-6">
        <ActionCard
          to={`/agents/${agentId}/persona`}
          icon="face"
          title={t('agentDetail.editPersona')}
          description={t('agentDetail.editPersonaDesc')}
        />
        <ActionCard
          to={`/agents/${agentId}/skills`}
          icon="psychology"
          title={t('agentDetail.manageSkills')}
          description={t('agentDetail.manageSkillsDesc')}
        />
        <ActionCard
          to={`/workspace/${agentId}`}
          icon="folder_open"
          title={t('agentDetail.workspace')}
          description={t('agentDetail.workspaceDesc')}
        />
        <ActionCard
          to={`/agents/${agentId}/memory`}
          icon="memory"
          title={t('agentDetail.manageMemory')}
          description={t('agentDetail.manageMemoryDesc')}
        />
        <ActionCard
          to={`/agents/${agentId}/sessions`}
          icon="forum"
          title={t('agentDetail.manageSessions')}
          description={t('agentDetail.manageSessionsDesc')}
        />
        <ActionCard
          to={`/agents/${agentId}/analytics`}
          icon="analytics"
          title={t('agentDetail.manageAnalytics')}
          description={t('agentDetail.manageAnalyticsDesc')}
        />
      </div>

      {/* Identity details */}
      <div className="card-outlined">
        <h2 className="section-title flex items-center gap-2">
          <span className="material-symbols-outlined text-xl text-primary">
            badge
          </span>
          {t('agentDetail.identity')}
        </h2>
        <div className="space-y-3">
          <DetailRow label={t('agentDetail.agentId')} value={agent.id} />
          <DetailRow label={t('agentDetail.name')} value={agent.name} />
          <DetailRow
            label={t('agentDetail.effectiveModel')}
            value={agent.model ?? t('agentDetail.useServerDefault')}
          />
        </div>
      </div>

      <LlmModelSection agentId={agentId} />

      {/* Discord Bot */}
      <DiscordBotSection agentId={agentId} />

      {/* Nostr sub-gateway */}
      <NostrSection agentId={agentId} />

      {/* Nostr 受信 → Discord 転記先 */}
      <NostrRelaySection agentId={agentId} />

      {/* MCP servers */}
      <McpSection agentId={agentId} />
    </>
  );
}
