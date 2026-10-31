import { useEffect, useState } from "react";
import { useI18n } from "../../i18n/store";
import { api } from "../../shared/api";
import { currentAppVersion } from "../../shared/native/appLifecycle";
import { Button } from "../../shared/ui/Button";
import { TitledCard } from "../../shared/ui/TitledCard";
import { useMessage } from "../../shared/ui/message";
import { PageContent } from "../../shell/layout/PageContent";
import styles from "./AboutPage.module.scss";

const REPOSITORY_URL = "https://github.com/tlkppm/ModelBridge";
const QQ_GROUP_NUMBER = "554549065";

export function AboutPage() {
  useI18n();
  const message = useMessage();
  const [version, setVersion] = useState<string | null>(null);
  const [versionFailed, setVersionFailed] = useState(false);

  useEffect(() => {
    let disposed = false;
    void currentAppVersion().then((next) => {
      if (!disposed) setVersion(next);
    }).catch((cause) => {
      if (disposed) return;
      setVersionFailed(true);
      message(t("读取版本失败：{error}", { error: cause instanceof Error ? cause.message : String(cause) }));
    });
    return () => { disposed = true; };
  }, [message]);

  const openRepository = () => {
    void api.openExternalUrl(REPOSITORY_URL).catch((cause) => {
      message(t("打开仓库失败：{error}", { error: cause instanceof Error ? cause.message : String(cause) }));
    });
  };

  const copyGroupNumber = () => {
    void api.copyCursorText(QQ_GROUP_NUMBER).then(() => {
      message(t("群号已复制"));
    }).catch((cause) => {
      message(t("复制群号失败：{error}", { error: cause instanceof Error ? cause.message : String(cause) }));
    });
  };

  return <PageContent title={t("关于")} sections={[
    {
      key: "application",
      estimatedHeight: 150,
      content: <TitledCard title="ModelBridge">
        <div className={styles.content}>
          <strong>{version ? t("当前版本 {version}", { version }) : versionFailed ? t("版本不可用") : t("正在读取版本")}</strong>
          <p>{t("连接 Cursor 与自定义模型服务的桌面网关。")}</p>
        </div>
      </TitledCard>,
    },
    {
      key: "repository",
      estimatedHeight: 180,
      content: <TitledCard title={t("项目仓库")} action={<Button size="small" onClick={openRepository}>{t("打开仓库")}</Button>}>
        <div className={styles.content}>
          <strong>tlkppm/ModelBridge</strong>
          <span className={styles.address}>{REPOSITORY_URL}</span>
          <p>{t("本项目基于 cursor-byok 独立维护，保留原始版权与 MIT 许可证。")}</p>
        </div>
      </TitledCard>,
    },
    {
      key: "community",
      estimatedHeight: 240,
      content: <TitledCard title={t("中国大陆交流频道")} action={<Button size="small" onClick={copyGroupNumber}>{t("复制群号")}</Button>}>
        <div className={styles.content}>
          <strong>Plus/Pro 分享群</strong>
          <div className={styles.groupNumber}><span>{t("QQ 群号")}</span><code>{QQ_GROUP_NUMBER}</code></div>
          <p>{t("面向中国大陆用户，交流模型接入、使用体验与版本反馈。")}</p>
          <p>{t("在 QQ 中搜索群号申请加入，入群以群设置和审核为准。")}</p>
          <p>{t("请勿分享密钥、验证码或私密对话内容。")}</p>
        </div>
      </TitledCard>,
    },
  ]} />;
}
