import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import Sidebar from './Sidebar';

function renderSidebar(initialPath = '/') {
  return render(
    <MemoryRouter initialEntries={[initialPath]}>
      <Sidebar />
    </MemoryRouter>,
  );
}

const navLabels = [
  'nav.dashboard',
  'nav.agents',
  'nav.sessions',
];

describe('Sidebar', () => {
  it('renders all 3 navigation links', () => {
    renderSidebar();
    for (const label of navLabels) {
      expect(screen.getByText(label)).toBeInTheDocument();
    }
  });

  it('renders the OpenCrab branding', () => {
    renderSidebar();
    expect(screen.getByText('brand.name')).toBeInTheDocument();
    expect(screen.getByText('brand.subtitle')).toBeInTheDocument();
  });

  it('marks Dashboard as active on "/"', () => {
    renderSidebar('/');
    const dashboardLink = screen.getByText('nav.dashboard').closest('a');
    expect(dashboardLink).toHaveClass('nav-item-active');
  });

  it('marks Agents as active on "/agents"', () => {
    renderSidebar('/agents');
    const agentsLink = screen.getByText('nav.agents').closest('a');
    expect(agentsLink).toHaveClass('nav-item-active');
  });

  it('marks Agents as active on "/agents/a1"', () => {
    renderSidebar('/agents/a1');
    const agentsLink = screen.getByText('nav.agents').closest('a');
    expect(agentsLink).toHaveClass('nav-item-active');
  });

  it('marks Sessions as active on "/sessions"', () => {
    renderSidebar('/sessions');
    const sessionsLink = screen.getByText('nav.sessions').closest('a');
    expect(sessionsLink).toHaveClass('nav-item-active');
  });

  it('does not mark Dashboard as active on "/agents"', () => {
    renderSidebar('/agents');
    const dashboardLink = screen.getByText('nav.dashboard').closest('a');
    expect(dashboardLink).toHaveClass('nav-item');
    expect(dashboardLink).not.toHaveClass('nav-item-active');
  });

  it('renders version info', () => {
    renderSidebar();
    expect(screen.getByText('brand.version')).toBeInTheDocument();
  });
});
