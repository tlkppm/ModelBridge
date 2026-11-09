import { useCallback, useEffect, useRef, useState } from "react";
import { api, type ImageGenerationKeyAction, type ImageGenerationMode, type ImageGenerationSettingsInput, type ImageGenerationSettingsView } from "../../shared/api";
import { useAppStore } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { Checkbox } from "../../shared/ui/Checkbox";
import { FormField, SecretTextInput, TextInput } from "../../shared/ui/FormControls";
import { ModelSelect } from "../../shared/ui/ModelSelect";
import { Combobox, Select, type ComboboxHandle } from "../../shared/ui/Select";
import { Switch } from "../../shared/ui/Switch";
import { TitledCard } from "../../shared/ui/TitledCard";
import { useMessage } from "../../shared/ui/message";
import { modelProviderName } from "../../shared/utils/modelProvider";
import styles from "./ImageGenerationSettingsCard.module.scss";

type ImageGenerationDraft = ImageGenerationSettingsInput & {
  key_action: ImageGenerationKeyAction;
  api_key: string;
};

function settingsDraft(view?: ImageGenerationSettingsView): ImageGenerationDraft {
  return {
    enabled: view?.enabled ?? false,
    mode: view?.mode ?? "existing",
    connection_model_hash: view?.connection_model_hash ?? "",
    base_url: view?.base_url ?? "",
    use_full_url: view?.use_full_url ?? false,
    image_model: view?.image_model ?? "",
    key_action: "keep",
    api_key: "",
  };
}

function urlOrigin(value: string) {
  try {
    return new URL(value.trim()).origin;
  } catch {
    return null;
  }
}

function errorText(cause: unknown) {
  return cause instanceof Error ? cause.message : String(cause);
}

export function ImageGenerationSettingsCard() {
  const { models } = useAppStore();
  const message = useMessage();
  const [saved, setSaved] = useState<ImageGenerationSettingsView | null>(null);
  const [draft, setDraft] = useState<ImageGenerationDraft>(() => settingsDraft());
  const [editing, setEditing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [loadVersion, setLoadVersion] = useState(0);
  const [modelOptions, setModelOptions] = useState<string[]>([]);
  const [discovering, setDiscovering] = useState(false);
  const [discoveryError, setDiscoveryError] = useState<string | null>(null);
  const [discovered, setDiscovered] = useState(false);
  const discoveryVersion = useRef(0);
  const modelCombobox = useRef<ComboboxHandle>(null);
  const connection = models.find((model) => model.type === "openai" && model.model_hash === draft.connection_model_hash);

  const clearDiscovery = useCallback(() => {
    discoveryVersion.current += 1;
    setModelOptions([]);
    setDiscoveryError(null);
    setDiscovered(false);
    setDiscovering(false);
  }, []);

  useEffect(() => {
    let active = true;
    setError(null);
    void api.imageGenerationSettings().then((view) => {
      if (!active) return;
      setSaved(view);
      setDraft(settingsDraft(view));
    }).catch((cause: unknown) => {
      if (active) setError(errorText(cause));
    });
    return () => {
      active = false;
      discoveryVersion.current += 1;
    };
  }, [loadVersion]);

  useEffect(clearDiscovery, [connection, clearDiscovery]);

  const changeConnection = (patch: Partial<ImageGenerationDraft>) => {
    clearDiscovery();
    setError(null);
    setDraft((current) => ({ ...current, ...patch }));
  };

  const savedOrigin = urlOrigin(saved?.base_url ?? "");
  const draftOrigin = urlOrigin(draft.base_url);
  const crossOrigin = draft.mode === "independent" && saved?.has_api_key
    && savedOrigin !== null && draftOrigin !== null && savedOrigin !== draftOrigin;
  const connectionError = draft.mode === "existing" && draft.connection_model_hash && !connection
    ? t("所选连接已失效或不是 OpenAI 兼容内置连接，请重新选择。")
    : null;

  const requestInput = (): ImageGenerationSettingsInput => {
    if (draft.mode === "independent") {
      if (draft.key_action === "replace" && !draft.api_key.trim()) {
        throw new Error(t("替换密钥时请输入非空 API 密钥。"));
      }
      if (crossOrigin && draft.key_action === "keep") {
        throw new Error(t("独立地址已跨源变更，必须替换密钥或显式清空，不能保留旧密钥。"));
      }
    }
    return {
      enabled: draft.enabled,
      mode: draft.mode,
      connection_model_hash: draft.connection_model_hash,
      base_url: draft.base_url.trim(),
      use_full_url: draft.use_full_url,
      image_model: draft.image_model.trim(),
      key_action: draft.mode === "existing" ? "keep" : draft.key_action,
      ...(draft.mode === "independent" && draft.key_action === "replace" ? { api_key: draft.api_key.trim() } : {}),
    };
  };

  const edit = () => {
    if (!saved) return;
    clearDiscovery();
    setDraft(settingsDraft(saved));
    setError(null);
    setEditing(true);
  };

  const cancel = () => {
    clearDiscovery();
    setDraft(settingsDraft(saved ?? undefined));
    setError(null);
    setEditing(false);
  };

  const save = async () => {
    if (!saved || saving) return;
    clearDiscovery();
    setSaving(true);
    setError(null);
    try {
      const view = await api.setImageGenerationSettings(requestInput());
      setSaved(view);
      setDraft(settingsDraft(view));
      setEditing(false);
      message(t("图片生成设置已保存"));
    } catch (cause) {
      setError(errorText(cause));
    } finally {
      setDraft((current) => ({ ...current, api_key: "" }));
      setSaving(false);
    }
  };

  const discover = async () => {
    if (!editing || saving || discovering) return;
    const version = ++discoveryVersion.current;
    setDiscovering(true);
    setDiscoveryError(null);
    setModelOptions([]);
    setDiscovered(false);
    try {
      const result = await api.discoverImageModels(requestInput());
      if (version !== discoveryVersion.current) return;
      const options = [...new Set(result.models.filter((model) => model.trim()))];
      setModelOptions(options);
      setDiscovered(true);
      if (options.length) modelCombobox.current?.openAll();
    } catch (cause) {
      if (version === discoveryVersion.current) setDiscoveryError(errorText(cause));
    } finally {
      if (version === discoveryVersion.current) setDiscovering(false);
    }
  };

  const connectionOptions = [
    { value: "", label: t("请选择已有连接"), group: t("OpenAI 兼容内置连接") },
    ...models.filter((model) => model.type === "openai").map((model) => ({
      value: model.model_hash,
      label: model.display_name && model.display_name !== model.model_id
        ? `${model.display_name} (${model.model_id})`
        : model.display_name || model.model_id,
      group: modelProviderName(model),
    })),
  ];
  const canDiscover = draft.mode === "existing" ? Boolean(connection) : Boolean(draft.base_url.trim());
  const modeLabel = (mode: ImageGenerationMode) => mode === "existing" ? t("已有连接") : t("独立 API");
  const action = editing ? <div className={styles.actions}>
    <Button size="small" disabled={saving} onClick={cancel}>{t("取消")}</Button>
    <Button size="small" variant="primary" disabled={saving} onClick={() => void save()}>
      {saving ? t("保存中…") : t("保存")}
    </Button>
  </div> : <Button size="small" disabled={!saved && !error} onClick={saved ? edit : () => setLoadVersion((current) => current + 1)}>
    {saved ? t("编辑") : error ? t("重试") : t("加载中…")}
  </Button>;

  return <TitledCard title={t("图片生成")} action={action}>
    <div className={styles.content}>
      <div className={styles.row}>
        <div className={styles.description}>
          <strong>{t("启用图片生成")}</strong>
          <small>{t("保存后供 Cursor 图片生成工具使用。切换开关不会发起网络请求。")}</small>
        </div>
        <Switch label={t("启用图片生成")} checked={draft.enabled} disabled={!editing || saving}
          onChange={(enabled) => setDraft((current) => ({ ...current, enabled }))} />
      </div>
      {error && <div className={styles.error} role="alert">{error}</div>}
      {editing ? <>
        <FormField label={t("连接方式")}>
          <Select ariaLabel={t("连接方式")} value={draft.mode} disabled={saving}
            options={[{ value: "existing", label: t("已有连接") }, { value: "independent", label: t("独立 API") }]}
            onChange={(mode) => changeConnection({ mode: mode as ImageGenerationMode, key_action: "keep", api_key: "" })} />
        </FormField>
        {draft.mode === "existing" ? <>
          <ModelSelect mode="single" label={t("已有连接")} value={draft.connection_model_hash}
            options={connectionOptions} disabled={saving} onChange={(connection_model_hash) => changeConnection({ connection_model_hash })} />
          <small className={styles.hint}>{t("仅借用连接的地址、密钥和自定义 Headers，不修改聊天模型标识。插件连接不受支持。")}</small>
          {connectionError && <div className={styles.error} role="alert">{connectionError}</div>}
          {!models.some((model) => model.type === "openai") && <small className={styles.hint}>{t("暂无 OpenAI 兼容内置连接，请先添加连接或改用独立 API。")}</small>}
        </> : <>
          <FormField label={draft.use_full_url ? t("完整生图请求 URL") : t("独立 API 地址")}>
            <TextInput value={draft.base_url} disabled={saving} autoComplete="off" spellCheck={false}
              placeholder={draft.use_full_url ? "https://api.example.com/v1/images/generations" : "https://api.example.com"}
              onChange={(event) => changeConnection({ base_url: event.target.value })} />
          </FormField>
          <Checkbox label={t("使用完整请求地址")} checked={draft.use_full_url} disabled={saving}
            onChange={(use_full_url) => changeConnection({ use_full_url })} />
          <small className={styles.hint}>{draft.use_full_url
            ? t("独立完整 URL 将原样用于生图请求，不追加路径。")
            : t("独立地址将自动拼接图片生成端点，已有版本路径不会重复添加。")}</small>
          <div className={styles.row}>
            <strong>{t("已保存的独立密钥")}</strong>
            <span className={styles.value}>{saved?.has_api_key ? t("已设置") : t("未设置")}</span>
          </div>
          <FormField label={t("密钥操作")}>
            <Select ariaLabel={t("密钥操作")} value={draft.key_action} disabled={saving}
              options={[{ value: "keep", label: t("保留已保存密钥") }, { value: "replace", label: t("替换密钥") }, { value: "clear", label: t("显式清空密钥") }]}
              onChange={(key_action) => changeConnection({ key_action: key_action as ImageGenerationKeyAction, api_key: "" })} />
          </FormField>
          {draft.key_action === "replace" && <FormField label={t("新的 API 密钥")}>
            <SecretTextInput value={draft.api_key} disabled={saving} autoComplete="new-password" spellCheck={false}
              onChange={(event) => changeConnection({ api_key: event.target.value })} />
          </FormField>}
          <small className={styles.hint}>{t("不会回显已保存密钥。空输入不代表清空；保存或取消后清除密钥草稿，保存失败时需重新输入。")}</small>
          {draft.key_action === "clear" && <small className={styles.hint}>{t("保存后将删除独立密钥；仅适用于无需密钥的上游。")}</small>}
          {crossOrigin && <div className={styles.warning} role="status">{t("独立地址已跨源变更，必须替换密钥或显式清空，不能保留旧密钥。")}</div>}
        </>}
        <FormField label={t("图片模型")}>
          <Combobox ref={modelCombobox} value={draft.image_model} options={modelOptions} disabled={saving}
            placeholder={t("手动输入图片模型标识，可留空")}
            onChange={(image_model) => setDraft((current) => ({ ...current, image_model }))}
            append={<Button disabled={saving || discovering || !canDiscover} onClick={() => void discover()}>
              {discovering ? t("获取中…") : t("获取模型")}
            </Button>} />
        </FormField>
        {discoveryError && <div className={styles.error} role="alert">
          {t("获取候选失败，仍可手动填写：{error}", { error: discoveryError })}
        </div>}
        {discovered && !modelOptions.length && <small className={styles.hint}>{t("上游未返回候选模型，仍可手动填写。")}</small>}
      </> : saved && <>
        <div className={styles.row}><strong>{t("连接方式")}</strong><span className={styles.value}>{modeLabel(saved.mode)}</span></div>
        {saved.mode === "existing" ? <>
          <div className={styles.row}><strong>{t("已有连接")}</strong><span className={styles.value}>
            {connectionOptions.find((option) => option.value === saved.connection_model_hash)?.label || saved.connection_model_hash || t("未设置")}
          </span></div>
          {connectionError && <div className={styles.error} role="alert">{connectionError}</div>}
        </> : <>
          <div className={styles.row}><strong>{saved.use_full_url ? t("完整生图请求 URL") : t("独立 API 地址")}</strong><span className={styles.value}>{saved.base_url || t("未设置")}</span></div>
          <div className={styles.row}><strong>{t("已保存的独立密钥")}</strong><span className={styles.value}>{saved.has_api_key ? t("已设置") : t("未设置")}</span></div>
        </>}
        <div className={styles.row}><strong>{t("图片模型")}</strong><span className={styles.value}>{saved.image_model || t("省略模型，由上游决定")}</span></div>
      </>}
      <small className={styles.hint}>{t("图片模型留空时请求不传 model，由上游决定默认模型；并非所有上游都支持。")}</small>
      <small className={styles.hint}>{t("获取模型只读取候选列表，不验证生图能力，也不会生成图片。发现失败不影响手动填写。")}</small>
      {saved && <div className={styles.savedStatus}>
        <div className={styles.row}><strong>{t("已保存配置状态")}</strong><span className={styles.value}>
          {saved.configured ? t("配置完整，未验证生图能力") : t("配置未完成")}
        </span></div>
        {saved.configuration_error && <div className={styles.error} role="alert">{t("已保存配置错误：{error}", { error: saved.configuration_error })}</div>}
        {saved.endpoint_url && <div className={styles.endpoint}>
          <strong>{t("已保存配置的生图地址")}</strong><code>{saved.endpoint_url}</code>
        </div>}
      </div>}
    </div>
  </TitledCard>;
}
