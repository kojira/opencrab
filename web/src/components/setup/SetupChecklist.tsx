import { useEffect, useState } from 'react';
import { Link } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { getSetupStatus, STEP_ORDER, type SetupStatus } from '../../api/setup';

/**
 * Home に常設するオンボーディング進捗カード。
 * 未完のステップがあれば「セットアップを続ける」導線を出す。
 * 全完了なら控えめな完了表示にする。
 */
export default function SetupChecklist() {
  const { t } = useTranslation();
  const [status, setStatus] = useState<SetupStatus | null>(null);

  useEffect(() => {
    getSetupStatus().then(setStatus).catch(() => {});
  }, []);

  if (!status) return null;

  const doneCount = STEP_ORDER.filter((k) => status.steps[k]?.done).length;

  if (status.complete) {
    return (
      <div className="flex items-center gap-2 px-4 py-2.5 rounded-xl bg-success/10 border border-success/30">
        <span className="material-symbols-outlined text-base text-success">task_alt</span>
        <span className="text-sm text-on-surface-variant">{t('setup.checklist.allDone')}</span>
      </div>
    );
  }

  return (
    <div className="card-outlined p-4 border-primary/40 bg-primary/5">
      <div className="flex items-center justify-between mb-3 flex-wrap gap-2">
        <div>
          <h2 className="text-base font-semibold text-on-surface">
            {t('setup.checklist.title')}
          </h2>
          <p className="text-xs text-on-surface-variant mt-0.5">
            {t('setup.checklist.progress', { done: doneCount, total: STEP_ORDER.length })}
          </p>
        </div>
        <Link to="/setup" className="btn-filled text-sm py-1.5 px-3">
          <span className="material-symbols-outlined text-base">rocket_launch</span>
          {t('setup.checklist.continue')}
        </Link>
      </div>
      <ul className="space-y-1.5">
        {STEP_ORDER.map((k) => {
          const done = status.steps[k]?.done ?? false;
          return (
            <li key={k} className="flex items-center gap-2">
              <span
                className={`material-symbols-outlined text-lg ${
                  done ? 'text-success' : 'text-on-surface-variant/50'
                }`}
              >
                {done ? 'check_circle' : 'radio_button_unchecked'}
              </span>
              <span
                className={`text-sm ${done ? 'text-on-surface-variant line-through' : 'text-on-surface'}`}
              >
                {t(`setup.step.${k}`)}
              </span>
            </li>
          );
        })}
      </ul>
    </div>
  );
}
