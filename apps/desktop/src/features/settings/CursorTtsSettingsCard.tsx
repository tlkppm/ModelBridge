import { useEffect, useState } from "react";
import { api, type CursorTtsSettings } from "../../shared/api";
import { Button } from "../../shared/ui/Button";
import { FormField, TextInput } from "../../shared/ui/FormControls";
import { TitledCard } from "../../shared/ui/TitledCard";
import { useMessage } from "../../shared/ui/message";
import styles from "./CursorTtsSettingsCard.module.scss";

export function CursorTtsSettingsCard() {
  const message = useMessage();
  const [saved, setSaved] = useState<CursorTtsSettings | null>(null);
  const [draft, setDraft] = useState<CursorTtsSettings>({ service_url: "" });
  const [editing, setEditing] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [loadVersion, setLoadVersion] = useState(0);

  useEffect(() => {
    let active = true;
    void api.cursorTtsSettings().then((settings) => {
      if (!active) return;
      setSaved(settings);
      setDraft(settings);
    }).catch((cause: unknown) => {
      if (active) setError(cause instanceof Error ? cause.message : String(cause));
    });
    return () => { active = false; };
  }, [loadVersion]);

  const edit = () => {
    if (!saved) return;
    setDraft(saved);
    setError(null);
    setEditing(true);
  };

  const cancel = () => {
    if (saved) setDraft(saved);
    setError(null);
    setEditing(false);
  };

  const save = async () => {
    if (!saved || saving) return;
    try {
      setSaving(true);
      setError(null);
      const next = await api.setCursorTtsSettings({ service_url: draft.service_url.trim() });
      setSaved(next);
      setDraft(next);
      setEditing(false);
      message(t("Cursor TTS 设置已保存"));
    } catch (cause) {
      const text = cause instanceof Error ? cause.message : String(cause);
      setError(text);
      message(text);
    } finally {
      setSaving(false);
    }
  };

  const action = editing ? <div className={styles.actions}>
    <Button size="small" disabled={saving} onClick={cancel}>{t("取消")}</Button>
    <Button size="small" variant="primary" disabled={saving} onClick={() => void save()}>
      {saving ? t("保存中…") : t("保存")}
    </Button>
  </div> : <Button size="small" disabled={!saved && !error} onClick={saved ? edit : () => setLoadVersion((current) => current + 1)}>
    {saved ? t("编辑") : error ? t("重试") : t("加载中…")}
  </Button>;

  return <TitledCard title={t("Cursor 语音试听")} action={action}>
    <div className={styles.content}>
      {error && <div className={styles.error} role="alert">{error}</div>}
      {editing ? <FormField
        label={t("本地 TTS 服务地址")}
        hint={t("填写本机 TTS HTTP 服务的基础地址，留空时继续使用 Cursor 官方 TTS。")}
      >
        <TextInput
          value={draft.service_url}
          disabled={saving}
          autoComplete="off"
          spellCheck={false}
          placeholder="http://127.0.0.1:5000"
          onChange={(event) => setDraft({ service_url: event.target.value })}
        />
      </FormField> : <div className={styles.row}>
        <div className={styles.description}>
          <strong>{t("本地 TTS 服务")}</strong>
          <small>{t("试听请求会发送到本机服务；未配置时使用 Cursor 官方 TTS。")}</small>
        </div>
        <span className={styles.value}>{saved?.service_url || t("未配置")}</span>
      </div>}
      <small className={styles.hint}>{t("服务需要接收 /aiserver.v1.AiService/TextToSpeech，并返回 Cursor 兼容的 protobuf 音频响应。")}</small>
    </div>
  </TitledCard>;
}
