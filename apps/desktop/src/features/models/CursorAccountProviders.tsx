import { useCallback, useEffect, useRef, useState } from "react";
import { api, type PluginOAuthBegin, type PluginResourceState, type PluginRuntimeStatus } from "../../shared/api";
import { appStore, useAppStore } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { Modal } from "../../shared/ui/Modal";
import { Switch } from "../../shared/ui/Switch";
import styles from "./CursorSettings.module.scss";

const accountProviders = [
  { pluginId: "dev.cursorbyok.examples.codex-auth", providerId: "codex", name: "ChatGPT (Codex)" },
  { pluginId: "dev.cursorbyok.plugins.antigravity-auth", providerId: "antigravity", name: "Antigravity" },
] as const;

const codexVerificationUrl = "https://auth.openai.com/codex/device";

type AccountProvider = (typeof accountProviders)[number];
type AccountAction = "refresh" | "runtime" | "account" | "sync" | "model";
type DeviceLogin =
  | { status: "starting" | "cancelled" | "expired" | "denied" | "failed" | "saved" }
  | { status: "waiting"; device: PluginOAuthBegin & { userCode: string }; retrying: boolean };
type ModelSync = "completed" | "failed" | "failedAfterLogin";

export function CursorAccountProviders() {
  const [selected, setSelected] = useState<AccountProvider | null>(null);
  return <>
    <div className={styles.accountEntries} role="group" aria-label={t("账号模型")}>
      {accountProviders.map((entry) => <Button key={entry.pluginId} onClick={() => setSelected(entry)}>{entry.name}</Button>)}
    </div>
    {selected && <CursorAccountPanel key={selected.pluginId} entry={selected} onClose={() => setSelected(null)} />}
  </>;
}

function CursorAccountPanel({ entry, onClose }: { entry: AccountProvider; onClose: () => void }) {
  const { plugins } = useAppStore();
  const [runtime, setRuntime] = useState<PluginRuntimeStatus | null>(null);
  const [action, setAction] = useState<AccountAction | null>(null);
  const [actionError, setActionError] = useState<Exclude<AccountAction, "sync"> | null>(null);
  const [catalogReadFailed, setCatalogReadFailed] = useState(false);
  const [login, setLogin] = useState<DeviceLogin | null>(null);
  const [modelSync, setModelSync] = useState<ModelSync | null>(null);
  const [deviceAction, setDeviceAction] = useState<"copied" | "copyFailed" | "openFailed" | null>(null);
  const currentAction = useRef<AccountAction | null>(null);
  const loginController = useRef<AbortController | null>(null);
  const plugin = plugins.find((item) => item.id === entry.pluginId);
  const provider = plugin?.providers.find((item) => item.id === entry.providerId);
  const resource = plugin?.resources.find((item) => item.type === provider?.resourceType);
  const accounts = resource?.resources ?? [];
  const models = provider?.models ?? [];
  const hasUsableAccount = accounts.some(({ state }) => accountStatus(state) === "ready");
  const hasInvalidAccount = accounts.some(({ state }) => state.status === "invalid");
  const isCodex = entry.providerId === "codex";
  const deviceMethod = isCodex ? resource?.add.find((method) => method.type === "oauth2.0" && method.id === "chatgpt-device") : undefined;
  const waitingForLogin = login?.status === "starting" || login?.status === "waiting";
  const controlsBusy = action !== null || waitingForLogin;
  const runtimeReady = runtime?.state === "ready";
  const canManage = runtimeReady && !catalogReadFailed && actionError !== "refresh";
  const runtimeInitializing = runtime?.state === "initializing";
  const needsRuntime = runtime?.state === "uninitialized" || runtime?.state === "failed";

  const run = useCallback(async (operation: AccountAction, task: () => Promise<void>) => {
    if (currentAction.current) return;
    currentAction.current = operation;
    setAction(operation);
    setActionError(null);
    try {
      await task();
    } catch {
      if (operation === "sync") setModelSync("failed");
      else setActionError(operation);
    } finally {
      currentAction.current = null;
      setAction(null);
    }
  }, []);

  const refreshCatalog = useCallback(async () => {
    try {
      await appStore.refreshPlugins();
      setCatalogReadFailed(false);
    } catch {
      setCatalogReadFailed(true);
    }
  }, []);

  const refreshStatus = useCallback(() => run("refresh", async () => {
    const next = await api.pluginRuntime();
    setRuntime(next);
    if (next.state !== "initializing") await refreshCatalog();
  }), [refreshCatalog, run]);

  useEffect(() => {
    void refreshStatus();
    setLogin((previous) => previous?.status === "starting" || previous?.status === "waiting" ? { status: "cancelled" } : previous);
    return () => {
      loginController.current?.abort();
      loginController.current = null;
    };
  }, [refreshStatus]);

  useEffect(() => {
    if (!runtimeInitializing || action || actionError === "refresh") return;
    const timer = window.setTimeout(() => void refreshStatus(), 800);
    return () => window.clearTimeout(timer);
  }, [action, actionError, refreshStatus, runtimeInitializing]);

  const changeRuntime = (initialize: boolean) => run("runtime", async () => {
    const next = await (initialize ? api.initializePluginRuntime() : api.cancelPluginRuntimeInitialization());
    setRuntime(next);
    if (next.state !== "initializing") await refreshCatalog();
  });

  const cancelLogin = () => {
    loginController.current?.abort();
    loginController.current = null;
    setLogin({ status: "cancelled" });
  };

  const startLogin = async () => {
    if (!deviceMethod || !resource || loginController.current || currentAction.current) return;
    const controller = new AbortController();
    loginController.current = controller;
    const { signal } = controller;
    let deadline: number | undefined;
    setLogin({ status: "starting" });
    setDeviceAction(null);
    setActionError(null);
    try {
      const begun = await api.pluginOAuthBegin(entry.pluginId, resource.type, deviceMethod.id, signal);
      if (signal.aborted) return;
      if (!begun.userCode || !begun.sessionId || begun.verificationUrl !== codexVerificationUrl || !Number.isFinite(begun.expiresAtMs)) {
        setLogin({ status: "failed" });
        return;
      }
      const device = { ...begun, userCode: begun.userCode };
      const remaining = device.expiresAtMs - Date.now();
      if (remaining <= 0) {
        setLogin({ status: "expired" });
        return;
      }
      deadline = window.setTimeout(() => {
        controller.abort();
        if (loginController.current === controller) {
          loginController.current = null;
          setLogin({ status: "expired" });
        }
      }, remaining);
      setLogin({ status: "waiting", device, retrying: false });
      let interval = device.pollIntervalMs;
      while (!signal.aborted) {
        await waitForPoll(interval, signal);
        if (signal.aborted) return;
        if (Date.now() >= device.expiresAtMs) {
          setLogin({ status: "expired" });
          return;
        }
        const result = await api.pluginOAuthPoll(device.sessionId, signal).catch(() => null);
        if (signal.aborted) return;
        if (!result || result.status === "pending") {
          interval = result?.pollIntervalMs ?? interval;
          setLogin({ status: "waiting", device, retrying: result === null });
          continue;
        }
        window.clearTimeout(deadline);
        if (result.status === "completed") {
          setLogin({ status: "saved" });
          setModelSync(result.modelSyncError ? "failedAfterLogin" : "completed");
          await run("refresh", refreshCatalog);
        } else {
          setLogin({ status: result.status });
        }
        return;
      }
    } catch {
      if (!signal.aborted) setLogin({ status: "failed" });
    } finally {
      window.clearTimeout(deadline);
      if (loginController.current === controller) loginController.current = null;
    }
  };

  const copyCode = async (code: string) => {
    const controller = loginController.current;
    try {
      await api.copyCursorText(code);
      if (loginController.current === controller) setDeviceAction("copied");
    } catch {
      if (loginController.current === controller) setDeviceAction("copyFailed");
    }
  };

  const openVerification = async () => {
    const controller = loginController.current;
    try {
      await api.openExternalUrl(codexVerificationUrl);
    } catch {
      if (loginController.current === controller) setDeviceAction("openFailed");
    }
  };

  return <Modal open title={t("{name} 账号与模型目录", { name: entry.name })} closeLabel={t("关闭")} onClose={() => {
    loginController.current?.abort();
    onClose();
  }}>
    <div className={styles.editor}>
      <section className={styles.accountSection}>
        <div className={styles.modelRow}>
          <div className={styles.accountStatus} aria-live="polite">
            <strong>{t("插件运行时")}</strong>
            <span>{runtimeLabel(runtime, action === "refresh")}</span>
          </div>
          <div className={styles.accountEntries}>
            <Button size="small" disabled={controlsBusy} onClick={() => void refreshStatus()}>{action === "refresh" ? t("刷新中…") : t("刷新状态")}</Button>
            {needsRuntime && <Button size="small" variant="primary" disabled={controlsBusy} onClick={() => void changeRuntime(true)}>{t("下载并初始化运行时")}</Button>}
            {runtimeInitializing && <Button size="small" disabled={controlsBusy} onClick={() => void changeRuntime(false)}>{t("取消初始化")}</Button>}
          </div>
        </div>
        {catalogReadFailed && <span role="alert">{t("无法读取账号与模型目录，请刷新状态。")}</span>}
        {actionError === "refresh" && <span role="alert">{t("无法读取插件运行时状态，请刷新状态。")}</span>}
        {actionError === "runtime" && <span role="alert">{t("运行时操作失败，请刷新状态后重试。")}</span>}
      </section>
      <section className={styles.accountSection}>
        <div className={styles.modelRow}>
          <div className={styles.accountStatus}>
            <strong>{t("账号")}</strong>
            <span>{!resource ? t("账号状态暂不可用") : accounts.length === 0 ? t("未登录") : t("已保存 {count} 个账号", { count: accounts.length })}</span>
          </div>
          {deviceMethod && <Button size="small" variant="primary" disabled={!canManage || controlsBusy} onClick={() => void startLogin()}>
            {hasInvalidAccount ? t("重新登录") : accounts.length ? t("添加账号") : t("使用 ChatGPT 登录")}
          </Button>}
        </div>
        {accounts.map((account) => <div className={styles.modelRow} key={account.id}>
          <div className={styles.accountStatus}>
            <strong>{account.displayName}</strong>
            <span>{accountLabel(account.state)}</span>
          </div>
          {resource?.canRefresh && <Button size="small" disabled={!canManage || controlsBusy} onClick={() => void run("account", async () => {
            await api.refreshPluginResource(entry.pluginId, resource.type, account.id);
            await refreshCatalog();
          })}>{t("刷新账号")}</Button>}
        </div>)}
        {action === "account" && <span role="status">{t("刷新中…")}</span>}
        {actionError === "account" && <span role="alert">{t("账号刷新失败，请重试。")}</span>}
        {!isCodex && <span>{t("Cloud Code 的公开支持范围尚未核实，首次绑定和重新绑定暂不可用。")}</span>}
        {isCodex && runtimeReady && !deviceMethod && <span>{t("设备授权入口暂不可用。")}</span>}
        {login && <div className={styles.accountSection}>
          <span role="status">{loginLabel(login)}</span>
          {login.status === "waiting" && <>
            <div className={styles.accountEntries}>
              <span>{t("设备验证码")}</span>
              <code className={styles.command}>{login.device.userCode}</code>
              <Button size="small" onClick={() => void copyCode(login.device.userCode)}>{deviceAction === "copied" ? t("已复制") : t("复制验证码")}</Button>
              <Button size="small" variant="primary" onClick={() => void openVerification()}>{t("打开授权网页")}</Button>
            </div>
            {deviceAction === "copyFailed" && <span role="alert">{t("无法复制设备验证码，可以手动选中并复制。")}</span>}
            {deviceAction === "openFailed" && <span role="alert">{t("无法打开授权网页，请重试。")}</span>}
          </>}
          {waitingForLogin && <div className={styles.accountEntries}>
            <Button size="small" onClick={cancelLogin}>{t("取消等待")}</Button>
          </div>}
        </div>}
      </section>
      <section className={styles.accountSection}>
        <div className={styles.modelRow}>
          <div className={styles.accountStatus}>
            <strong>{t("模型目录")}</strong>
            <span>{!provider?.hasModels ? t("模型目录暂不可用") : models.length === 0 ? t("尚未同步模型") : t("已启用 {enabled} / {total} 个模型", { enabled: models.filter((model) => model.enabled).length, total: models.length })}</span>
          </div>
          {provider?.hasModels && <Button size="small" disabled={!canManage || !hasUsableAccount || controlsBusy} onClick={() => void run("sync", async () => {
            setModelSync(null);
            await api.syncPluginModels(entry.pluginId, provider.id);
            setModelSync("completed");
            await refreshCatalog();
          })}>{action === "sync" ? t("正在同步模型…") : t("同步模型")}</Button>}
        </div>
        {provider?.hasModels && !hasUsableAccount && <span>{t("同步模型需要可用账号。")}</span>}
        {!isCodex && provider?.hasModels && <span>{t("已有目录可能包含插件的内置模型列表，Cloud Code 调用权限需单独验证。")}</span>}
        {modelSync && <span role={modelSync === "completed" ? "status" : "alert"}>{modelSyncLabel(modelSync)}</span>}
        {actionError === "model" && <span role="alert">{t("无法更新模型启用状态，请重试。")}</span>}
        {models.map((model) => <div className={styles.modelRow} key={model.id}>
          <div className={styles.accountStatus}>
            <strong>{model.displayName}</strong>
            <span>{model.modelId}</span>
          </div>
          <Switch checked={model.enabled} disabled={!canManage || controlsBusy} label={t("启用 {model}", { model: model.displayName })} onChange={(enabled) => void run("model", async () => {
            await api.setPluginModelEnabled(entry.pluginId, model.providerId, model.modelId, enabled);
            await refreshCatalog();
          })} />
        </div>)}
      </section>
    </div>
  </Modal>;
}

function waitForPoll(interval: number, signal: AbortSignal) {
  return new Promise<void>((resolve) => {
    const finish = () => {
      window.clearTimeout(timer);
      signal.removeEventListener("abort", finish);
      resolve();
    };
    const timer = window.setTimeout(finish, Number.isFinite(interval) ? Math.max(1000, interval) : 1000);
    signal.addEventListener("abort", finish, { once: true });
  });
}

function runtimeLabel(runtime: PluginRuntimeStatus | null, refreshing: boolean) {
  switch (runtime?.state) {
    case "uninitialized": return t("尚未初始化插件运行时");
    case "initializing": return t("正在初始化插件运行时…");
    case "ready": return t("插件运行时已就绪");
    case "failed": return t("插件运行时初始化失败");
    case "unsupported": return t("当前环境不支持插件运行时");
    default: return refreshing ? t("正在检查插件运行时") : t("插件运行时状态未知");
  }
}

function accountStatus(state: PluginResourceState) {
  return state.status === "cooling" && state.retryAtMs != null && state.retryAtMs <= Date.now() ? "ready" : state.status;
}

function accountLabel(state: PluginResourceState) {
  switch (accountStatus(state)) {
    case "ready": return t("账号可用");
    case "cooling": return t("账号冷却中");
    case "invalid": return t("账号授权已失效");
  }
}

function loginLabel(login: DeviceLogin) {
  switch (login.status) {
    case "starting": return t("正在申请设备验证码…");
    case "waiting": return login.retrying ? t("暂时无法查询授权状态，将自动重试。") : t("在授权网页输入设备验证码后，等待授权结果。");
    case "cancelled": return t("已停止等待授权。如果刚完成网页授权，可以刷新账号状态。");
    case "expired": return t("设备验证码已过期，请重新登录。");
    case "denied": return t("设备授权被拒绝，请重新登录。");
    case "failed": return t("设备授权失败，请重新登录。");
    case "saved": return t("登录成功，账号已保存。");
  }
}

function modelSyncLabel(state: ModelSync) {
  switch (state) {
    case "completed": return t("模型目录已同步。");
    case "failed": return t("模型同步失败，请重试。");
    case "failedAfterLogin": return t("账号已保存，模型同步失败。可以单独重试同步。");
  }
}
