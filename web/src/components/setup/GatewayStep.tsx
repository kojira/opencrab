import { useTranslation } from 'react-i18next';
import type { GatewayStepStatus } from '../../api/setup';
import CopyBlock from './CopyBlock';
import {
  BLUESKY_GATEWAY_REPO,
  CLI_GATEWAY_README,
  GATEWAYS_REPO,
  ONBOARDING_DOC,
  buildCommand,
  placementCommand,
  replCommand,
} from './cliGateway';

// ---- Step 3: connect a gateway to the gate socket ----
export default function GatewayStep({
  gateway,
  agentId,
}: {
  gateway: GatewayStepStatus | undefined;
  agentId: string | null;
}) {
  const { t } = useTranslation();
  const socket = gateway?.gate_socket ?? '';
  const connected = gateway?.connected ?? [];

  return (
    <div className="space-y-5">
      <div>
        <h2 className="text-lg font-semibold text-on-surface">{t('setup.gateway.title')}</h2>
        <p className="text-body-sm text-on-surface-variant mt-1">{t('setup.gateway.desc')}</p>
        <p className="text-body-sm text-on-surface-variant mt-1">
          {t('setup.gateway.repos')}{' '}
          <a className="text-primary underline" href={GATEWAYS_REPO} target="_blank" rel="noreferrer">
            kojira/opencrab-gateways
          </a>{' '}
          (Discord / Nostr / Web / CLI),{' '}
          <a
            className="text-primary underline"
            href={BLUESKY_GATEWAY_REPO}
            target="_blank"
            rel="noreferrer"
          >
            kojira/opencrab-bluesky-gateway
          </a>{' '}
          (Bluesky)
        </p>
      </div>

      <ConnectedList connected={connected} />

      {socket === '' ? (
        <div className="flex items-center gap-2 p-3 rounded-md bg-error-container">
          <span className="material-symbols-outlined text-error text-base">error</span>
          <p className="text-body-sm text-error-on-container">{t('setup.gateway.noSocket')}</p>
        </div>
      ) : (
        <div className="space-y-3">
          <CopyBlock label={t('setup.gateway.socket')} text={socket} />
          {agentId ? (
            <CopyBlock label={t('setup.gateway.agentId')} text={agentId} />
          ) : (
            <p className="text-body-sm text-on-surface-variant">{t('setup.gateway.noAgent')}</p>
          )}
        </div>
      )}

      {socket !== '' && agentId && (
        <div className="space-y-3">
          <h3 className="text-title-md text-on-surface">{t('setup.gateway.cliTitle')}</h3>
          <CopyBlock label={t('setup.gateway.cliBuild')} text={buildCommand()} />
          <CopyBlock
            label={t('setup.gateway.cliPlacement')}
            text={placementCommand(socket, agentId)}
          />
          <CopyBlock label={t('setup.gateway.cliRun')} text={replCommand(agentId)} />
          <p className="text-body-sm text-on-surface-variant">{t('setup.gateway.cliRev')}</p>
        </div>
      )}

      <p className="text-body-sm text-on-surface-variant">
        <a className="text-primary underline" href={CLI_GATEWAY_README} target="_blank" rel="noreferrer">
          {t('setup.gateway.cliReadme')}
        </a>
        {' · '}
        <a className="text-primary underline" href={ONBOARDING_DOC} target="_blank" rel="noreferrer">
          docs/onboarding.md
        </a>
      </p>
    </div>
  );
}

function ConnectedList({ connected }: { connected: GatewayStepStatus['connected'] }) {
  const { t } = useTranslation();
  if (connected.length === 0) {
    return (
      <div className="flex items-center gap-2 p-3 rounded-md bg-primary/10">
        <span className="material-symbols-outlined text-primary text-base">hourglass_empty</span>
        <p className="text-body-sm text-on-surface-variant">{t('setup.gateway.waiting')}</p>
      </div>
    );
  }
  return (
    <div className="p-3 rounded-md bg-success/15 space-y-1">
      <p className="text-body-sm text-success font-semibold">
        {t('setup.gateway.connected', { count: connected.length })}
      </p>
      <ul className="space-y-0.5">
        {connected.map((c) => (
          <li key={c.instance_id} className="text-xs font-mono text-on-surface-variant">
            {c.instance_id} → {c.agent_id ?? '?'}
          </li>
        ))}
      </ul>
    </div>
  );
}
