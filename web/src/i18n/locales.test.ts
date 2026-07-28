import { describe, it, expect } from 'vitest';
import en from './locales/en.json';
import ja from './locales/ja.json';

/**
 * 辞書の腐り検知。
 *
 * このリポジトリは「コードが参照している翻訳キーが辞書に無い」「片方の言語にだけ
 * 追加した」という壊れ方を何度も踏んでいる。i18next は未解決キーをそのまま
 * 画面に出す（= レビューでもテストでも気づきにくい）ため、静的に検査する。
 */

/** src 配下の全ソースを生テキストで取り込む（node:fs を使わず Vite に解決させる）。 */
const sources = import.meta.glob('../**/*.{ts,tsx}', {
  query: '?raw',
  import: 'default',
  eager: true,
}) as Record<string, string>;

/**
 * 静的に決まる翻訳キーだけを集める。
 * - t('foo.bar') / t("foo.bar")
 * - labelKey: 'foo.bar'（ナビゲーション定義。t() 経由で解決される）
 *
 * t(`foo.${x}`) や t(variable) のような動的キーは静的検査できないので対象外。
 * それらは呼び出し側で defaultValue を渡す運用になっている。
 */
function staticKeys(src: string): string[] {
  const keys: string[] = [];
  for (const m of src.matchAll(/\bt\(\s*(['"])([^'"]+?)\1/g)) keys.push(m[2]);
  for (const m of src.matchAll(/\blabelKey\s*:\s*(['"])([^'"]+?)\1/g)) keys.push(m[2]);
  // 文字列連結による動的キー（例: t('agentStatus.' + status)）は末尾が '.' になる。
  return keys.filter((k) => !k.endsWith('.'));
}

const referenced = new Map<string, Set<string>>();
for (const [file, src] of Object.entries(sources)) {
  if (/\.test\.tsx?$/.test(file)) continue;
  for (const key of staticKeys(src)) {
    if (!referenced.has(key)) referenced.set(key, new Set());
    referenced.get(key)!.add(file.replace(/^\.\.\//, ''));
  }
}

const dicts = { en, ja } as Record<string, Record<string, string>>;

describe('i18n 辞書', () => {
  it('走査対象のソースから翻訳キーを収集できている', () => {
    // 正規表現が壊れて 0 件になったまま「全部通った」となるのを防ぐ番人。
    expect(referenced.size).toBeGreaterThan(100);
  });

  for (const lang of Object.keys(dicts)) {
    it(`コードが参照する全キーが ${lang}.json に存在する`, () => {
      const missing = [...referenced.entries()]
        .filter(([key]) => !(key in dicts[lang]))
        .map(([key, files]) => `${key}  <- ${[...files].join(', ')}`);
      expect(missing, `${lang}.json に欠落:\n${missing.join('\n')}`).toEqual([]);
    });
  }

  it('en と ja のキー集合が一致する', () => {
    const enKeys = Object.keys(en).sort();
    const jaKeys = Object.keys(ja).sort();
    expect(jaKeys.filter((k) => !(k in en)), 'ja のみに存在').toEqual([]);
    expect(enKeys.filter((k) => !(k in ja)), 'en のみに存在').toEqual([]);
  });

  it('補間プレースホルダが en と ja で一致する', () => {
    const placeholders = (s: string) =>
      [...s.matchAll(/\{\{(\w+)\}\}/g)].map((m) => m[1]).sort();
    const mismatched = Object.keys(en)
      .filter((k) => k in ja)
      .filter(
        (k) =>
          placeholders(en[k as keyof typeof en]).join(',') !==
          placeholders(ja[k as keyof typeof ja]).join(','),
      );
    expect(mismatched, `プレースホルダ不一致: ${mismatched.join(', ')}`).toEqual([]);
  });

  it('エージェント詳細タブのラベルが両言語に揃っている', () => {
    // 実害の再発防止: 11 タブのラベルが欠けるとアイコンだけになり判別できない。
    const navKeys = Object.keys(en).filter((k) => k.startsWith('agentNav.'));
    expect(navKeys).toHaveLength(11);
    for (const k of navKeys) {
      expect(ja[k as keyof typeof ja], `${k} が ja.json に無い`).toBeTruthy();
      expect(en[k as keyof typeof en], `${k} が en.json に無い`).toBeTruthy();
    }
  });
});
