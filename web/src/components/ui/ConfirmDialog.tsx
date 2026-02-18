import { useTranslation } from 'react-i18next';

interface Props {
  title: string;
  message: string;
  confirmLabel?: string;
  onConfirm: () => void;
  onCancel: () => void;
}

export default function ConfirmDialog({
  title,
  message,
  confirmLabel,
  onConfirm,
  onCancel,
}: Props) {
  const { t } = useTranslation();

  return (
    <div className="scrim" onClick={onCancel}>
      <div className="dialog" onClick={(e) => e.stopPropagation()}>
        <div className="flex items-center gap-3 mb-4">
          <span className="material-symbols-outlined text-2xl text-error">
            warning
          </span>
          <h3 className="text-title-lg text-on-surface">{title}</h3>
        </div>
        <p className="text-body-lg text-on-surface-variant mb-6">{message}</p>
        <div className="flex gap-3 justify-end">
          <button className="btn-outlined" onClick={onCancel}>
            {t('common.cancel')}
          </button>
          <button className="btn-danger" onClick={onConfirm}>
            <span className="material-symbols-outlined text-xl">delete</span>
            {confirmLabel ?? t('common.delete')}
          </button>
        </div>
      </div>
    </div>
  );
}
