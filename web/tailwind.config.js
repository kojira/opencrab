/** @type {import('tailwindcss').Config} */
export default {
  content: ["./index.html", "./src/**/*.{js,ts,jsx,tsx}"],
  darkMode: "class",
  theme: {
    extend: {
      colors: {
        primary: {
          DEFAULT: '#4F46E5',
          container: '#E0E7FF',
          'on': '#FFFFFF',
          'on-container': '#312E81',
        },
        secondary: {
          DEFAULT: '#475569',
          container: '#E2E8F0',
          'on': '#FFFFFF',
          'on-container': '#1E293B',
        },
        tertiary: {
          DEFAULT: '#0D9488',
          container: '#CCFBF1',
          'on': '#FFFFFF',
          'on-container': '#134E4A',
        },
        surface: {
          DEFAULT: '#FAFAFA',
          container: '#FFFFFF',
          'container-high': '#F1F5F9',
          variant: '#E2E8F0',
          dim: '#0F172A',
          'dim-container': '#1E293B',
        },
        'on-surface': {
          DEFAULT: '#0F172A',
          variant: '#475569',
        },
        error: {
          DEFAULT: '#DC2626',
          container: '#FEE2E2',
          'on-container': '#991B1B',
        },
        success: {
          DEFAULT: '#16A34A',
          container: '#DCFCE7',
          'on-container': '#166534',
        },
        warning: {
          DEFAULT: '#D97706',
          container: '#FEF3C7',
          'on-container': '#92400E',
        },
        outline: {
          DEFAULT: '#CBD5E1',
          variant: '#E2E8F0',
        },
      },
      fontFamily: {
        sans: ['Inter', 'system-ui', 'sans-serif'],
        mono: ['JetBrains Mono', 'monospace'],
      },
      fontSize: {
        'display-lg': ['3.5625rem', { lineHeight: '4rem', letterSpacing: '-0.015em', fontWeight: '400' }],
        'display-md': ['2.8125rem', { lineHeight: '3.25rem', letterSpacing: '0', fontWeight: '400' }],
        'display-sm': ['2.25rem', { lineHeight: '2.75rem', letterSpacing: '0', fontWeight: '400' }],
        'headline-lg': ['2rem', { lineHeight: '2.5rem', letterSpacing: '0', fontWeight: '400' }],
        'headline-md': ['1.75rem', { lineHeight: '2.25rem', letterSpacing: '0', fontWeight: '400' }],
        'headline-sm': ['1.5rem', { lineHeight: '2rem', letterSpacing: '0', fontWeight: '400' }],
        'title-lg': ['1.375rem', { lineHeight: '1.75rem', letterSpacing: '0', fontWeight: '500' }],
        'title-md': ['1rem', { lineHeight: '1.5rem', letterSpacing: '0.009em', fontWeight: '500' }],
        'title-sm': ['0.875rem', { lineHeight: '1.25rem', letterSpacing: '0.007em', fontWeight: '500' }],
        'body-lg': ['1rem', { lineHeight: '1.5rem', letterSpacing: '0.009em', fontWeight: '400' }],
        'body-md': ['0.875rem', { lineHeight: '1.25rem', letterSpacing: '0.016em', fontWeight: '400' }],
        'body-sm': ['0.75rem', { lineHeight: '1rem', letterSpacing: '0.025em', fontWeight: '400' }],
        'label-lg': ['0.875rem', { lineHeight: '1.25rem', letterSpacing: '0.006em', fontWeight: '500' }],
        'label-md': ['0.75rem', { lineHeight: '1rem', letterSpacing: '0.031em', fontWeight: '500' }],
        'label-sm': ['0.6875rem', { lineHeight: '1rem', letterSpacing: '0.031em', fontWeight: '500' }],
      },
      boxShadow: {
        'elevation-1': '0 1px 2px 0 rgba(0,0,0,0.3), 0 1px 3px 1px rgba(0,0,0,0.15)',
        'elevation-2': '0 1px 2px 0 rgba(0,0,0,0.3), 0 2px 6px 2px rgba(0,0,0,0.15)',
        'elevation-3': '0 4px 8px 3px rgba(0,0,0,0.15), 0 1px 3px 0 rgba(0,0,0,0.3)',
        'elevation-4': '0 6px 10px 4px rgba(0,0,0,0.15), 0 2px 3px 0 rgba(0,0,0,0.3)',
        'elevation-5': '0 8px 12px 6px rgba(0,0,0,0.15), 0 4px 4px 0 rgba(0,0,0,0.3)',
      },
      borderRadius: {
        'xs': '0.25rem',
        'sm': '0.5rem',
        'md': '0.75rem',
        'lg': '1rem',
        'xl': '1.75rem',
        'full': '9999px',
      },
    },
  },
  plugins: [],
};
