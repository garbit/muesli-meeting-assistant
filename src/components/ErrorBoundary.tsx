import { Component, type ErrorInfo, type ReactNode } from "react";

interface Props {
  children: ReactNode;
}
interface State {
  error: Error | null;
}

/** Keeps one broken screen from unmounting the whole app. */
export default class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("render error", error, info.componentStack);
  }

  render() {
    if (this.state.error) {
      return (
        <div className="m-8 rounded-md border border-red-200 bg-red-50 p-4 text-sm text-red-800 dark:border-red-900 dark:bg-red-950 dark:text-red-200">
          <p className="font-medium">This screen hit an error.</p>
          <pre className="mt-2 whitespace-pre-wrap text-xs">{String(this.state.error.message || this.state.error)}</pre>
          <button onClick={() => this.setState({ error: null })} className="mt-3 rounded-md border border-red-300 px-2 py-1 text-xs">
            Try again
          </button>
        </div>
      );
    }
    return this.props.children;
  }
}
