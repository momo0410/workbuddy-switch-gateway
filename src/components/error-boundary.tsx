import { Component, type ErrorInfo, type ReactNode } from "react";
import { AlertTriangle, RotateCcw } from "lucide-react";

import { Button } from "@/components/ui/button";

interface Props {
  children: ReactNode;
  /** 兜底 UI标题（不同边界可区分，便于定位是哪块崩溃）。 */
  label?: string;
}

interface State {
  error: Error | null;
}

/**
 * 页面级错误边界：捕获子树渲染期抛出的致命错误，显示可恢复的错误面板 +
 * 「重试」按钮，而不是让整窗永久白屏。
 *
 * 背景：issue #36 中账号管理页因后端把加密信封对象 `{ $wbEncrypted, envelope }`
 * 透传为 `note`，前端直接渲染该对象触发 React #31（"Objects are not valid as a
 * React child"），整窗白屏且无法自救。加边界后，即使将来再有类似数据异常，
 * 用户也能看到明确错误并一键重试，而不必杀进程。
 *
 * 注意：错误边界是 class 组件（React 规定 hooks 无法捕获渲染错误）。
 */
export class PageErrorBoundary extends Component<Props, State> {
  state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    // 落到控制台，方便用户按 F12 把堆栈贴给开发者。
    console.error("[PageErrorBoundary] 页面渲染崩溃：", error, info.componentStack);
  }

  handleRetry = () => {
    this.setState({ error: null });
  };

  render() {
    const { error } = this.state;
    if (!error) return this.props.children;

    return (
      <div className="flex min-h-full flex-col items-center justify-center gap-4 p-10 text-center" role="alert">
        <div className="flex max-w-md flex-col items-center">
          <AlertTriangle className="size-10 text-destructive" aria-hidden="true" />
          <h2 className="mt-3 text-lg font-semibold text-foreground">页面渲染出错</h2>
          <p className="mt-2 text-sm leading-6 text-muted-foreground">
            本页遇到了意外的渲染错误。可点击下方按钮重试；若反复出现，请在「设置」中导出日志并反馈，附上错误信息有助于定位。
          </p>
          <pre className="mt-3 max-h-44 w-full overflow-auto rounded-md bg-muted p-3 text-left text-xs text-muted-foreground">
            {String(error?.message || error)}
          </pre>
          <Button onClick={this.handleRetry} className="mt-4 gap-1.5">
            <RotateCcw className="size-4" />
            重试
          </Button>
        </div>
      </div>
    );
  }
}
