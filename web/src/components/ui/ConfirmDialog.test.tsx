import { describe, it, expect, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import ConfirmDialog from './ConfirmDialog';

const defaultProps = {
  title: 'Delete Agent',
  message: 'Are you sure you want to delete this agent?',
  onConfirm: vi.fn(),
  onCancel: vi.fn(),
};

describe('ConfirmDialog', () => {
  it('renders title and message', () => {
    render(<ConfirmDialog {...defaultProps} />);
    expect(screen.getByText('Delete Agent')).toBeInTheDocument();
    expect(
      screen.getByText('Are you sure you want to delete this agent?'),
    ).toBeInTheDocument();
  });

  it('renders default confirm label from translation key', () => {
    render(<ConfirmDialog {...defaultProps} />);
    expect(screen.getByText('common.delete')).toBeInTheDocument();
  });

  it('renders custom confirm label', () => {
    render(<ConfirmDialog {...defaultProps} confirmLabel="Remove" />);
    expect(screen.getByText('Remove')).toBeInTheDocument();
  });

  it('calls onConfirm when confirm button is clicked', async () => {
    const onConfirm = vi.fn();
    const user = userEvent.setup();

    render(<ConfirmDialog {...defaultProps} onConfirm={onConfirm} />);
    await user.click(screen.getByText('common.delete'));

    expect(onConfirm).toHaveBeenCalledOnce();
  });

  it('calls onCancel when cancel button is clicked', async () => {
    const onCancel = vi.fn();
    const user = userEvent.setup();

    render(<ConfirmDialog {...defaultProps} onCancel={onCancel} />);
    await user.click(screen.getByText('common.cancel'));

    expect(onCancel).toHaveBeenCalledOnce();
  });

  it('calls onCancel when scrim (backdrop) is clicked', async () => {
    const onCancel = vi.fn();
    const user = userEvent.setup();

    const { container } = render(
      <ConfirmDialog {...defaultProps} onCancel={onCancel} />,
    );
    const scrim = container.querySelector('.scrim')!;
    await user.click(scrim);

    expect(onCancel).toHaveBeenCalledOnce();
  });
});
