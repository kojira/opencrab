import { useState, useEffect, useCallback } from 'react';
import { Link, useParams } from 'react-router-dom';
import {
  listWorkspace,
  readWorkspaceFile,
  writeWorkspaceFile,
} from '../api/workspace';
import type { WorkspaceEntryDto } from '../api/types';

function FileEntry({
  entry,
  onClick,
}: {
  entry: WorkspaceEntryDto;
  onClick: (name: string, isDir: boolean) => void;
}) {
  let icon: string;
  let iconColor: string;
  if (entry.is_dir) {
    icon = 'folder';
    iconColor = 'text-warning';
  } else {
    const ext = entry.name.split('.').pop() ?? '';
    switch (ext) {
      case 'rs':
        icon = 'code';
        iconColor = 'text-primary';
        break;
      case 'toml':
      case 'json':
      case 'yaml':
      case 'yml':
        icon = 'settings';
        iconColor = 'text-tertiary';
        break;
      case 'md':
      case 'txt':
        icon = 'article';
        iconColor = 'text-secondary';
        break;
      default:
        icon = 'draft';
        iconColor = 'text-on-surface-variant';
    }
  }

  return (
    <button
      className="w-full flex items-center gap-3 px-3 py-2.5 rounded-md hover:bg-secondary-container/40 active:bg-secondary-container/60 text-left transition-colors duration-150"
      onClick={() => onClick(entry.name, entry.is_dir)}
    >
      <span className={`material-symbols-outlined text-xl ${iconColor}`}>
        {icon}
      </span>
      <span className="flex-1 text-body-md text-on-surface">{entry.name}</span>
      {!entry.is_dir && (
        <span className="text-label-sm text-on-surface-variant">
          {entry.size} B
        </span>
      )}
    </button>
  );
}

export default function Workspace() {
  const { agentId } = useParams<{ agentId: string }>();
  const [currentPath, setCurrentPath] = useState('');
  const [entries, setEntries] = useState<WorkspaceEntryDto[] | null>(null);
  const [entriesError, setEntriesError] = useState<string | null>(null);
  const [selectedFile, setSelectedFile] = useState<string | null>(null);
  const [fileContent, setFileContent] = useState<string | null>(null);
  const [editing, setEditing] = useState(false);
  const [editContent, setEditContent] = useState('');

  const loadEntries = useCallback(() => {
    if (!agentId) return;
    setEntries(null);
    setEntriesError(null);
    listWorkspace(agentId, currentPath)
      .then(setEntries)
      .catch((e: Error) => setEntriesError(e.message));
  }, [agentId, currentPath]);

  useEffect(() => {
    loadEntries();
  }, [loadEntries]);

  const handleEntryClick = async (name: string, isDir: boolean) => {
    if (isDir) {
      setCurrentPath(
        currentPath ? `${currentPath}/${name}` : name,
      );
    } else {
      if (!agentId) return;
      const filePath = currentPath ? `${currentPath}/${name}` : name;
      setSelectedFile(name);
      try {
        const content = await readWorkspaceFile(agentId, filePath);
        setFileContent(content);
        setEditContent(content);
        setEditing(false);
      } catch (e) {
        setFileContent(`Error: ${e instanceof Error ? e.message : e}`);
      }
    }
  };

  const goUp = () => {
    const parent = currentPath.includes('/')
      ? currentPath.substring(0, currentPath.lastIndexOf('/'))
      : '';
    setCurrentPath(parent);
  };

  const handleSave = async () => {
    if (!agentId || !selectedFile) return;
    const filePath = currentPath
      ? `${currentPath}/${selectedFile}`
      : selectedFile;
    await writeWorkspaceFile(agentId, filePath, editContent);
    setFileContent(editContent);
    setEditing(false);
  };

  return (
    <div className="max-w-7xl mx-auto">
      <div className="flex items-center gap-3 mb-2">
        <Link to={`/agents/${agentId}`} className="btn-text p-2">
          <span className="material-symbols-outlined">arrow_back</span>
        </Link>
        <h1 className="page-title">Workspace</h1>
      </div>
      <div className="flex items-center gap-2 text-body-md text-on-surface-variant mb-6">
        <span className="material-symbols-outlined text-lg">smart_toy</span>
        <span>Agent: </span>
        <span className="font-mono text-on-surface">{agentId}</span>
      </div>

      <div className="grid grid-cols-1 lg:grid-cols-2 gap-6">
        {/* File tree panel */}
        <div className="card-outlined overflow-hidden">
          <div className="px-4 py-3 border-b border-outline-variant bg-surface-container-high">
            <div className="flex items-center gap-2">
              <span className="material-symbols-outlined text-lg text-primary">
                folder
              </span>
              {currentPath && (
                <button
                  className="btn-text text-body-sm p-1"
                  onClick={goUp}
                >
                  <span className="material-symbols-outlined text-lg">
                    arrow_upward
                  </span>
                  Up
                </button>
              )}
              <span className="text-label-lg text-on-surface-variant">
                /{currentPath}
              </span>
            </div>
          </div>

          <div className="p-1">
            {entriesError ? (
              <div className="p-4">
                <div className="flex items-center gap-2">
                  <span className="material-symbols-outlined text-error">
                    error
                  </span>
                  <p className="text-body-md text-error">
                    Error: {entriesError}
                  </p>
                </div>
              </div>
            ) : entries === null ? (
              <div className="p-8 text-center">
                <p className="text-body-md text-on-surface-variant">
                  Loading...
                </p>
              </div>
            ) : entries.length === 0 ? (
              <div className="p-8 text-center">
                <span className="material-symbols-outlined text-3xl text-on-surface-variant/40 mb-2">
                  folder_off
                </span>
                <p className="text-body-md text-on-surface-variant">
                  Empty directory
                </p>
              </div>
            ) : (
              entries.map((entry) => (
                <FileEntry
                  key={entry.name}
                  entry={entry}
                  onClick={handleEntryClick}
                />
              ))
            )}
          </div>
        </div>

        {/* File viewer / editor panel */}
        <div className="card-outlined overflow-hidden">
          <div className="px-4 py-3 border-b border-outline-variant bg-surface-container-high flex items-center justify-between">
            <div className="flex items-center gap-2">
              <span className="material-symbols-outlined text-lg text-primary">
                {selectedFile ? 'description' : 'draft'}
              </span>
              <span className="text-label-lg text-on-surface">
                {selectedFile ?? 'No file selected'}
              </span>
            </div>
            {selectedFile && (
              <button
                className={
                  editing
                    ? 'btn-outlined text-body-sm py-1.5 px-3'
                    : 'btn-tonal text-body-sm py-1.5 px-3'
                }
                onClick={() => setEditing(!editing)}
              >
                <span className="material-symbols-outlined text-lg">
                  {editing ? 'close' : 'edit'}
                </span>
                {editing ? 'Cancel' : 'Edit'}
              </button>
            )}
          </div>

          <div className="p-4">
            {fileContent != null ? (
              editing ? (
                <>
                  <textarea
                    className="w-full h-96 px-3 py-2 font-mono text-body-sm border border-outline rounded-md bg-surface text-on-surface focus:border-primary focus:ring-2 focus:ring-primary/20 focus:outline-none"
                    value={editContent}
                    onChange={(e) => setEditContent(e.target.value)}
                  />
                  <button className="btn-filled mt-3" onClick={handleSave}>
                    <span className="material-symbols-outlined text-xl">
                      save
                    </span>
                    Save
                  </button>
                </>
              ) : (
                <pre className="h-96 overflow-auto font-mono text-body-sm text-on-surface whitespace-pre-wrap p-2 rounded-md bg-surface-container-high">
                  {fileContent}
                </pre>
              )
            ) : (
              <div className="text-center py-16">
                <span className="material-symbols-outlined text-5xl text-on-surface-variant/30 mb-3">
                  description
                </span>
                <p className="text-body-lg text-on-surface-variant">
                  Select a file to view
                </p>
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
