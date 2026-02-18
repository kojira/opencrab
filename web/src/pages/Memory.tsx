import { useState, useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { getAgents } from '../api/agents';
import { getCuratedMemories, searchMemory } from '../api/memory';
import type {
  AgentSummary,
  CuratedMemoryDto,
  SessionLogResult,
} from '../api/types';

export default function Memory() {
  const { t } = useTranslation();
  const [agents, setAgents] = useState<AgentSummary[] | null>(null);
  const [selectedAgent, setSelectedAgent] = useState<string | null>(null);
  const [activeTab, setActiveTab] = useState<'curated' | 'search'>('curated');
  const [curated, setCurated] = useState<CuratedMemoryDto[] | null>(null);
  const [curatedError, setCuratedError] = useState<string | null>(null);
  const [searchQuery, setSearchQuery] = useState('');
  const [searchResults, setSearchResults] = useState<
    SessionLogResult[] | null
  >(null);
  const [searchError, setSearchError] = useState<string | null>(null);

  useEffect(() => {
    getAgents().then(setAgents).catch(() => {});
  }, []);

  useEffect(() => {
    if (!selectedAgent) return;
    setCurated(null);
    setCuratedError(null);
    getCuratedMemories(selectedAgent)
      .then(setCurated)
      .catch((e: Error) => setCuratedError(e.message));
  }, [selectedAgent]);

  const handleSearch = async () => {
    if (!selectedAgent || !searchQuery.trim()) return;
    setSearchError(null);
    try {
      const results = await searchMemory(selectedAgent, searchQuery);
      setSearchResults(results);
    } catch (e) {
      setSearchError(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <div className="max-w-7xl mx-auto">
      <h1 className="page-title mb-6">{t('memory.title')}</h1>

      {/* Agent selector */}
      <div className="card-elevated mb-6">
        <label className="block text-label-lg text-on-surface mb-2">
          <span className="flex items-center gap-1.5">
            <span className="material-symbols-outlined text-lg">
              smart_toy
            </span>
            {t('common.selectAgent')}
          </span>
        </label>
        {agents ? (
          <select
            className="select-outlined"
            onChange={(e) =>
              setSelectedAgent(e.target.value || null)
            }
          >
            <option value="">{t('common.selectAgentPlaceholder')}</option>
            {agents.map((a) => (
              <option key={a.id} value={a.id}>
                {a.name}
              </option>
            ))}
          </select>
        ) : (
          <p className="text-body-md text-on-surface-variant">
            {t('common.loadingAgents')}
          </p>
        )}
      </div>

      {selectedAgent ? (
        <>
          {/* Segmented tab switcher */}
          <div className="flex justify-center mb-6">
            <div className="segmented-group">
              <button
                className={
                  activeTab === 'curated'
                    ? 'segmented-btn-active'
                    : 'segmented-btn'
                }
                onClick={() => setActiveTab('curated')}
              >
                <span className="material-symbols-outlined text-lg mr-1.5">
                  auto_awesome
                </span>
                {t('memory.curatedMemory')}
              </button>
              <button
                className={
                  activeTab === 'search'
                    ? 'segmented-btn-active'
                    : 'segmented-btn'
                }
                onClick={() => setActiveTab('search')}
              >
                <span className="material-symbols-outlined text-lg mr-1.5">
                  search
                </span>
                {t('memory.searchLogs')}
              </button>
            </div>
          </div>

          {activeTab === 'curated' ? (
            curatedError ? (
              <div className="card-outlined border-error bg-error-container/30 p-4">
                <div className="flex items-center gap-2">
                  <span className="material-symbols-outlined text-error">
                    error
                  </span>
                  <p className="text-body-lg text-error-on-container">
                    {t('common.error', { message: curatedError })}
                  </p>
                </div>
              </div>
            ) : curated === null ? (
              <div className="empty-state">
                <p className="text-body-lg text-on-surface-variant">
                  {t('common.loading')}
                </p>
              </div>
            ) : curated.length === 0 ? (
              <div className="empty-state">
                <span className="material-symbols-outlined empty-state-icon">
                  memory
                </span>
                <p className="empty-state-text">
                  {t('memory.noCurated')}
                </p>
              </div>
            ) : (
              <div className="space-y-3">
                {curated.map((memory) => (
                  <div key={memory.id} className="card-outlined">
                    <div className="flex items-center justify-between mb-3">
                      <span className="badge-info">
                        <span className="material-symbols-outlined text-sm mr-0.5">
                          label
                        </span>
                        {memory.category}
                      </span>
                      <span className="text-label-sm text-on-surface-variant font-mono">
                        {memory.id}
                      </span>
                    </div>
                    <p className="text-body-lg text-on-surface whitespace-pre-wrap">
                      {memory.content}
                    </p>
                  </div>
                ))}
              </div>
            )
          ) : (
            <>
              {/* Search interface */}
              <div className="card-elevated mb-6">
                <div className="flex gap-3">
                  <div className="relative flex-1">
                    <span className="material-symbols-outlined absolute left-3 top-1/2 -translate-y-1/2 text-on-surface-variant">
                      search
                    </span>
                    <input
                      type="text"
                      className="input-outlined pl-11"
                      placeholder={t('memory.searchPlaceholder')}
                      value={searchQuery}
                      onChange={(e) => setSearchQuery(e.target.value)}
                      onKeyDown={(e) => e.key === 'Enter' && handleSearch()}
                    />
                  </div>
                  <button className="btn-filled" onClick={handleSearch}>
                    <span className="material-symbols-outlined text-xl">
                      search
                    </span>
                    {t('common.search')}
                  </button>
                </div>
              </div>

              {searchError ? (
                <div className="card-outlined border-error bg-error-container/30 p-4">
                  <div className="flex items-center gap-2">
                    <span className="material-symbols-outlined text-error">
                      error
                    </span>
                    <p className="text-body-lg text-error-on-container">
                      {t('common.error', { message: searchError })}
                    </p>
                  </div>
                </div>
              ) : searchResults ? (
                searchResults.length === 0 ? (
                  <div className="empty-state">
                    <span className="material-symbols-outlined empty-state-icon">
                      search_off
                    </span>
                    <p className="empty-state-text">{t('memory.noResults')}</p>
                  </div>
                ) : (
                  <div className="space-y-3">
                    <p className="text-label-lg text-on-surface-variant mb-2">
                      <span className="material-symbols-outlined text-lg mr-1 align-middle">
                        info
                      </span>
                      {t('memory.resultCount', { count: searchResults.length })}
                    </p>
                    {searchResults.map((log) => (
                      <div key={log.id} className="card-outlined">
                        <div className="flex justify-between mb-2">
                          <div className="flex items-center gap-2">
                            <span className="material-symbols-outlined text-lg text-primary">
                              person
                            </span>
                            <span className="text-label-lg text-on-surface">
                              {log.session_id}
                            </span>
                          </div>
                          <div className="flex items-center gap-2">
                            <span className="badge-neutral text-label-sm">
                              {log.log_type}
                            </span>
                            <span className="text-body-sm text-on-surface-variant">
                              {log.created_at}
                            </span>
                          </div>
                        </div>
                        <p className="text-body-lg text-on-surface whitespace-pre-wrap pl-8">
                          {log.content}
                        </p>
                      </div>
                    ))}
                  </div>
                )
              ) : null}
            </>
          )}
        </>
      ) : (
        <div className="empty-state">
          <span className="material-symbols-outlined empty-state-icon">
            memory
          </span>
          <p className="empty-state-text">
            {t('memory.selectAgent')}
          </p>
        </div>
      )}
    </div>
  );
}
