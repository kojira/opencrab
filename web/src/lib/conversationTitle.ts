/** 未指定 name の theme は address（= logical id）。UI は ID を表示名にしない。 */
export function conversationTitle(id: string, theme: string, unnamed: string): string {
  return theme === id ? unnamed : theme;
}
