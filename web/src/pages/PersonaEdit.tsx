import { useState, useEffect, useCallback, useRef } from 'react';
import { Link, useParams } from 'react-router-dom';
import { useTranslation } from 'react-i18next';
import { getAgent, updateSoul, listSoulPresets, createSoulPreset, deleteSoulPreset, applySoulPreset } from '../api/agents';
import type { SoulPresetDto } from '../api/types';

export default function PersonaEdit() {
  const { t } = useTranslation();
  const { id } = useParams<{ id: string }>();
  const [personaName, setPersonaName] = useState('');
  const [customTraits, setCustomTraits] = useState('');
  const [initialized, setInitialized] = useState(false);

  // Toast
  const [toast, setToast] = useState<{ message: string; isError?: boolean; key: number } | null>(null);
  const toastKey = useRef(0);

  // Preset state
  const [presets, setPresets] = useState<SoulPresetDto[]>([]);
  const [showPresetInput, setShowPresetInput] = useState(false);
  const [presetNameInput, setPresetNameInput] = useState('');
  const [confirmApplyId, setConfirmApplyId] = useState<string | null>(null);
  const [savingPreset, setSavingPreset] = useState(false);
  const composingRef = useRef(false);

  const showToast = useCallback((message: string, isError = false) => {
    toastKey.current += 1;
    setToast({ message, isError, key: toastKey.current });
  }, []);

  // Auto-dismiss toast
  useEffect(() => {
    if (!toast) return;
    const timer = setTimeout(() => setToast(null), 3000);
    return () => clearTimeout(timer);
  }, [toast?.key]);

  const loadPresets = useCallback(async () => {
    if (!id) return;
    const list = await listSoulPresets(id);
    setPresets(list);
  }, [id]);

  useEffect(() => {
    if (!id) return;
    getAgent(id).then((detail) => {
      setPersonaName(detail.persona_name);
      setCustomTraits(detail.custom_traits_json || '');
      setInitialized(true);
    });
    loadPresets();
  }, [id, loadPresets]);

  const handleSave = async () => {
    if (!id) return;
    try {
      await updateSoul(id, {
        persona_name: personaName,
        social_style_json: '{}',
        personality_json: '{}',
        thinking_style_json: '{}',
        custom_traits_json: customTraits || null,
      });
      showToast(t('personaEdit.savedSuccess'));
    } catch (e) {
      showToast(`Error: ${e instanceof Error ? e.message : e}`, true);
    }
  };

  const handleSavePreset = async () => {
    if (!id || !presetNameInput.trim()) return;
    setSavingPreset(true);
    try {
      await updateSoul(id, {
        persona_name: personaName,
        social_style_json: '{}',
        personality_json: '{}',
        thinking_style_json: '{}',
        custom_traits_json: customTraits || null,
      });
      await createSoulPreset(id, presetNameInput.trim());
      setPresetNameInput('');
      setShowPresetInput(false);
      showToast(t('personaEdit.presetSaved'));
      await loadPresets();
    } finally {
      setSavingPreset(false);
    }
  };

  const handleDeletePreset = async (presetId: string) => {
    if (!id) return;
    await deleteSoulPreset(id, presetId);
    showToast(t('personaEdit.presetDeleted'));
    await loadPresets();
  };

  const handleApplyPreset = async (presetId: string) => {
    if (!id) return;
    await applySoulPreset(id, presetId);
    const detail = await getAgent(id);
    setPersonaName(detail.persona_name);
    setCustomTraits(detail.custom_traits_json || '');
    setConfirmApplyId(null);
    showToast(t('personaEdit.presetApplied'));
  };

  if (!initialized) {
    return (
      <div className="empty-state">
        <p className="text-body-lg text-on-surface-variant">{t('common.loading')}</p>
      </div>
    );
  }

  return (
    <div className="max-w-4xl mx-auto">
      {/* Toast notification */}
      {toast && (
        <div
          className={`fixed top-4 left-1/2 -translate-x-1/2 z-50 flex items-center gap-2 px-5 py-3 rounded-lg shadow-lg ${
            toast.isError
              ? 'bg-error-container text-on-error-container'
              : 'bg-success-container text-success-on-container'
          }`}
          style={{ minWidth: '200px' }}
        >
          <span className={`material-symbols-outlined ${toast.isError ? 'text-error' : 'text-success'}`}>
            {toast.isError ? 'error' : 'check_circle'}
          </span>
          <p className="text-body-md">{toast.message}</p>
        </div>
      )}

      <div className="flex items-center gap-3 mb-6">
        <Link to={`/agents/${id}`} className="btn-text p-2">
          <span className="material-symbols-outlined">arrow_back</span>
        </Link>
        <h1 className="page-title">{t('personaEdit.title')}</h1>
      </div>

      {/* Presets section */}
      <div className="card-outlined mb-6">
        <div className="flex items-center justify-between mb-3">
          <h2 className="section-title flex items-center gap-2 mb-0">
            <span className="material-symbols-outlined text-xl text-primary">
              bookmarks
            </span>
            {t('personaEdit.presets')}
          </h2>
          {!showPresetInput && (
            <button
              className="btn-filled-tonal px-3 py-1.5 text-sm flex items-center gap-1"
              onClick={() => setShowPresetInput(true)}
            >
              <span className="material-symbols-outlined text-base">add</span>
              {t('personaEdit.saveAsPreset')}
            </button>
          )}
        </div>

        {/* Save preset input */}
        {showPresetInput && (
          <div className="flex items-center gap-2 mb-3">
            <input
              type="text"
              className="input-outlined text-sm px-3 py-1.5 flex-1"
              placeholder={t('personaEdit.presetNamePrompt')}
              value={presetNameInput}
              onChange={(e) => setPresetNameInput(e.target.value)}
              onCompositionStart={() => { composingRef.current = true; }}
              onCompositionEnd={() => { composingRef.current = false; }}
              onKeyDown={(e) => {
                if (composingRef.current) return;
                if (e.key === 'Enter') handleSavePreset();
                if (e.key === 'Escape') { setShowPresetInput(false); setPresetNameInput(''); }
              }}
              disabled={savingPreset}
              autoFocus
            />
            <button
              className="btn-filled-tonal px-3 py-1.5 text-sm"
              onClick={handleSavePreset}
              disabled={savingPreset}
            >
              {savingPreset ? t('common.saving') : t('common.save')}
            </button>
            <button
              className="btn-text px-2 py-1.5 text-sm"
              onClick={() => { setShowPresetInput(false); setPresetNameInput(''); }}
              disabled={savingPreset}
            >
              {t('common.cancel')}
            </button>
          </div>
        )}

        {/* Preset list */}
        {presets.length === 0 ? (
          <p className="text-body-sm text-on-surface-variant">
            {t('personaEdit.noPresets')}
          </p>
        ) : (
          <div className="flex flex-col gap-1">
            {presets.map((preset) => (
              <div
                key={preset.id}
                className="flex items-center justify-between px-3 py-2 rounded-lg hover:bg-surface-container-high transition-colors"
              >
                {confirmApplyId === preset.id ? (
                  <div className="flex items-center gap-2 w-full">
                    <span className="text-body-sm text-on-surface-variant flex-1">
                      {t('personaEdit.applyConfirm')}
                    </span>
                    <button
                      className="btn-filled px-3 py-1 text-sm"
                      onClick={() => handleApplyPreset(preset.id)}
                    >
                      OK
                    </button>
                    <button
                      className="btn-text px-2 py-1 text-sm"
                      onClick={() => setConfirmApplyId(null)}
                    >
                      {t('common.cancel')}
                    </button>
                  </div>
                ) : (
                  <>
                    <div className="flex items-center gap-2 min-w-0">
                      <span className="material-symbols-outlined text-base text-on-surface-variant">person</span>
                      <span className="text-body-md font-medium truncate">{preset.preset_name}</span>
                      <span className="text-body-sm text-on-surface-variant truncate">
                        — {preset.persona_name}
                      </span>
                    </div>
                    <div className="flex items-center gap-1 ml-2 flex-shrink-0">
                      <button
                        className="btn-filled-tonal px-3 py-1 text-sm flex items-center gap-1"
                        onClick={() => setConfirmApplyId(preset.id)}
                      >
                        <span className="material-symbols-outlined text-base">swap_horiz</span>
                        {t('personaEdit.apply')}
                      </button>
                      <button
                        className="btn-text px-2 py-1 text-sm text-error"
                        onClick={() => handleDeletePreset(preset.id)}
                      >
                        <span className="material-symbols-outlined text-base">delete</span>
                      </button>
                    </div>
                  </>
                )}
              </div>
            ))}
          </div>
        )}
      </div>

      {/* Persona name section */}
      <div className="card-outlined mb-6">
        <h2 className="section-title flex items-center gap-2">
          <span className="material-symbols-outlined text-xl text-primary">
            face
          </span>
          {t('personaEdit.personaName')}
        </h2>
        <input
          type="text"
          className="input-outlined"
          value={personaName}
          onChange={(e) => setPersonaName(e.target.value)}
        />
      </div>

      {/* Custom traits (Markdown) section */}
      <div className="card-outlined mb-6">
        <h2 className="section-title flex items-center gap-2">
          <span className="material-symbols-outlined text-xl text-primary">
            edit_note
          </span>
          {t('personaEdit.customTraits')}
        </h2>
        <p className="text-body-sm text-on-surface-variant mb-3">
          {t('personaEdit.customTraitsDesc')}
        </p>
        <textarea
          className="input-outlined w-full font-mono text-sm"
          rows={16}
          placeholder={t('personaEdit.customTraitsPlaceholder')}
          value={customTraits}
          onChange={(e) => setCustomTraits(e.target.value)}
        />
      </div>

      <button className="btn-filled w-full py-3" onClick={handleSave}>
        <span className="material-symbols-outlined text-xl">save</span>
        {t('common.save')}
      </button>
    </div>
  );
}
