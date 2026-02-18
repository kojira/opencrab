import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import SkillEditor from './SkillEditor';
import type { SkillDto } from '../../api/types';

function makeSkill(overrides: Partial<SkillDto> = {}): SkillDto {
  return {
    id: 'sk1',
    agent_id: 'a1',
    name: 'Summarization',
    description: 'Summarize text concisely',
    situation_pattern: '*',
    guidance: 'Be brief',
    source_type: 'standard',
    source_context: null,
    file_path: null,
    effectiveness: 0.85,
    usage_count: 42,
    is_active: true,
    ...overrides,
  };
}

describe('SkillEditor', () => {
  it('displays skill name and description', () => {
    render(<SkillEditor skill={makeSkill()} onToggle={vi.fn()} />);
    expect(screen.getByText('Summarization')).toBeInTheDocument();
    expect(screen.getByText('Summarize text concisely')).toBeInTheDocument();
  });

  it('displays usage count', () => {
    render(<SkillEditor skill={makeSkill({ usage_count: 10 })} onToggle={vi.fn()} />);
    expect(screen.getByText('skillEditor.usedTimes')).toBeInTheDocument();
  });

  it('displays effectiveness percentage', () => {
    render(<SkillEditor skill={makeSkill({ effectiveness: 0.75 })} onToggle={vi.fn()} />);
    expect(screen.getByText('skillEditor.effectiveness')).toBeInTheDocument();
  });

  it('calls onToggle with skill id and toggled state on click', async () => {
    const onToggle = vi.fn();
    const user = userEvent.setup();

    render(
      <SkillEditor skill={makeSkill({ id: 'sk2', is_active: true })} onToggle={onToggle} />,
    );

    const toggleButton = screen.getByRole('button');
    await user.click(toggleButton);

    expect(onToggle).toHaveBeenCalledWith('sk2', false);
  });

  it('sends is_active=true when toggling an inactive skill', async () => {
    const onToggle = vi.fn();
    const user = userEvent.setup();

    render(
      <SkillEditor skill={makeSkill({ id: 'sk3', is_active: false })} onToggle={onToggle} />,
    );

    await user.click(screen.getByRole('button'));
    expect(onToggle).toHaveBeenCalledWith('sk3', true);
  });

  it('displays source type badge', () => {
    render(<SkillEditor skill={makeSkill({ source_type: 'acquired' })} onToggle={vi.fn()} />);
    expect(screen.getByText('acquired')).toBeInTheDocument();
  });
});
