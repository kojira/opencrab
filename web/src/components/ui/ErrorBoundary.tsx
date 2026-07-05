import { Component, type ReactNode } from 'react';
import i18n from '../../i18n';

interface Props {
  children: ReactNode;
}

interface State {
  error: Error | null;
}

/**
 * ページ単位のクラッシュがダッシュボード全体を白画面にするのを防ぐ。
 * （実例: メモリページの API 型ミスマッチで app 全体が落ちていた）
 */
export default class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error) {
    console.error('Page crashed:', error);
  }

  render() {
    if (this.state.error) {
      return (
        <div className="card-outlined border-error bg-error-container/30 p-6 max-w-2xl mx-auto mt-8">
          <div className="flex items-start gap-3">
            <span className="material-symbols-outlined text-error">error</span>
            <div className="min-w-0">
              <p className="text-title-md text-error-on-container mb-1">
                {i18n.t('errorBoundary.title')}
              </p>
              <pre className="text-body-sm text-on-surface-variant whitespace-pre-wrap break-words font-mono">
                {this.state.error.message}
              </pre>
              <button
                className="btn-tonal mt-3"
                onClick={() => this.setState({ error: null })}
              >
                {i18n.t('errorBoundary.retry')}
              </button>
            </div>
          </div>
        </div>
      );
    }
    return this.props.children;
  }
}
