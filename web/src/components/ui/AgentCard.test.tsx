import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import AgentCard from './AgentCard';
import type { AgentSummary } from '../../api/types';

function makeAgent(overrides: Partial<AgentSummary> = {}): AgentSummary {
  return {
    id: 'a1',
    name: 'Alice',
    persona_name: 'Curious Alice',
    role: 'discussant',
    image_url: null,
    status: 'active',
    skill_count: 3,
    session_count: 5,
    ...overrides,
  };
}

function renderCard(agent: AgentSummary) {
  return render(
    <MemoryRouter>
      <AgentCard agent={agent} />
    </MemoryRouter>,
  );
}

describe('AgentCard', () => {
  it('displays agent name and persona name', () => {
    renderCard(makeAgent());
    expect(screen.getByText('Alice')).toBeInTheDocument();
    expect(screen.getByText('Curious Alice')).toBeInTheDocument();
  });

  it('displays skill and session counts', () => {
    renderCard(makeAgent({ skill_count: 7, session_count: 12 }));
    expect(screen.getByText('7 skills')).toBeInTheDocument();
    expect(screen.getByText('12 sessions')).toBeInTheDocument();
  });

  it('shows status badge with text', () => {
    renderCard(makeAgent({ status: 'active' }));
    expect(screen.getByText('active')).toBeInTheDocument();
  });

  it('applies badge-success class for active status', () => {
    renderCard(makeAgent({ status: 'active' }));
    const badge = screen.getByText('active').closest('span');
    expect(badge).toHaveClass('badge-success');
  });

  it('applies badge-neutral class for idle status', () => {
    renderCard(makeAgent({ status: 'idle' }));
    const badge = screen.getByText('idle').closest('span');
    expect(badge).toHaveClass('badge-neutral');
  });

  it('applies badge-error class for error status', () => {
    renderCard(makeAgent({ status: 'error' }));
    const badges = screen.getAllByText('error');
    const badgeSpan = badges.find((el) => el.className.includes('badge'));
    expect(badgeSpan).toHaveClass('badge-error');
  });

  it('links to the agent detail page', () => {
    renderCard(makeAgent({ id: 'abc123' }));
    const link = screen.getByRole('link');
    expect(link).toHaveAttribute('href', '/agents/abc123');
  });

  it('shows first character avatar when no image_url', () => {
    renderCard(makeAgent({ name: 'Bob', image_url: null }));
    expect(screen.getByText('B')).toBeInTheDocument();
  });

  it('shows image when image_url is provided', () => {
    renderCard(makeAgent({ image_url: 'https://example.com/pic.png' }));
    const img = screen.getByRole('img');
    expect(img).toHaveAttribute('src', 'https://example.com/pic.png');
  });
});
