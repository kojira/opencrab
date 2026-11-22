import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import type { ConversationStepStatus } from '../../api/setup';
import NewConversationButton from '../ui/NewConversationButton';
import CopyBlock from './CopyBlock';
import { replCommand } from './cliGateway';

// ---- Step 4: the first conversation ----
export default function ConversationStep({
  conversation,
  agentId,
}: {
  conversation: ConversationStepStatus | undefined;
  agentId: string | null;
}) {
  const { t } = useTranslation();
  const sessionId = conversation?.done ? conversation.session_id : null;

  return (
    <div className="space-y-5">
      <div>
        <h2 className="text-lg font-semibold text-on-surface">{t('setup.conversation.title')}</h2>
        <p className="text-body-sm text-on-surface-variant mt-1">
          {t('setup.conversation.desc')}
        </p>
      </div>

      {sessionId ? (
        <div className="space-y-3">
          <div className="flex items-center gap-2 p-3 rounded-md bg-success/15">
            <span className="material-symbols-outlined text-success text-base">check_circle</span>
            <p className="text-body-sm text-success">{t('setup.conversation.done')}</p>
          </div>
          <Link to={`/sessions/${sessionId}`} className="btn-tonal text-sm inline-flex">
            {t('setup.conversation.openSession')}
          </Link>
        </div>
      ) : (
        <div className="flex items-center gap-2 p-3 rounded-md bg-primary/10">
          <span className="material-symbols-outlined text-primary text-base">hourglass_empty</span>
          <p className="text-body-sm text-on-surface-variant">{t('setup.conversation.waiting')}</p>
        </div>
      )}

      <div className="space-y-2">
        <h3 className="text-title-md text-on-surface">{t('setup.conversation.webTitle')}</h3>
        <p className="text-body-sm text-on-surface-variant">{t('setup.conversation.webDesc')}</p>
        <NewConversationButton agentId={agentId} />
      </div>

      {agentId && (
        <div className="space-y-2">
          <h3 className="text-title-md text-on-surface">{t('setup.conversation.cliTitle')}</h3>
          <p className="text-body-sm text-on-surface-variant">{t('setup.conversation.cliDesc')}</p>
          <CopyBlock text={replCommand(agentId)} />
        </div>
      )}
    </div>
  );
}
