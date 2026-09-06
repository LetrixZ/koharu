'use client'

import { Eraser } from 'lucide-react'
import { useEffect, useId, useState } from 'react'
import { useTranslation } from 'react-i18next'

import {
  PreferencePage,
  PreferenceRow,
  PreferenceSection,
} from '@/components/preferences/PreferenceFields'
import { applyApi, refreshApi } from '@/lib/backend'
import { useKoharuStore } from '@/lib/store'
import type { CredentialInput } from '@koharu/bridge/protocol'
import { Button } from '@koharu/ui/components/button'
import { Input } from '@koharu/ui/components/input'
import { Switch } from '@koharu/ui/components/switch'

export function ApiPreferences() {
  const { t } = useTranslation()
  const settings = useKoharuStore((state) => state.api)
  const [enabled, setEnabled] = useState(false)
  const [host, setHost] = useState('')
  const [port, setPort] = useState('')
  const [credential, setCredential] = useState<CredentialInput | null>(null)
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    void refreshApi().catch(() => undefined)
  }, [])

  useEffect(() => {
    if (settings) {
      setEnabled(settings.enabled)
      setHost(settings.host)
      setPort(String(settings.port))
      setCredential(settings.token)
    }
  }, [settings])

  if (!settings || !credential) {
    return <p className='py-10 text-[12px] text-muted-foreground'>{t('settings.loading')}</p>
  }

  const portNumber = Number(port)
  const portValid = Number.isInteger(portNumber) && portNumber >= 1 && portNumber <= 65535
  const changed =
    enabled !== settings.enabled || host !== settings.host || port !== String(settings.port)
  const configured = !credential.clear && (credential.configured || Boolean(credential.value))

  const handleApply = async () => {
    setBusy(true)
    try {
      const result = await applyApi(
        enabled,
        portValid ? portNumber : settings.port,
        host.trim() || settings.host,
        credential,
      )
      setEnabled(result.enabled)
      setHost(result.host)
      setPort(String(result.port))
      setCredential(result.token)
    } finally {
      setBusy(false)
    }
  }

  return (
    <PreferencePage title={t('settings.api.title')} description={t('settings.api.description')}>
      <div>
        <PreferenceSection title={t('settings.api.section')}>
          <PreferenceRow
            title={t('settings.api.enabled')}
            description={t('settings.api.enabledDescription')}
          >
            <Switch checked={enabled} onCheckedChange={setEnabled} />
          </PreferenceRow>
          <PreferenceRow
            title={t('settings.api.host')}
            description={t('settings.api.hostDescription')}
          >
            <Input
              type='text'
              autoComplete='off'
              autoCapitalize='none'
              spellCheck={false}
              value={host}
              disabled={!enabled}
              placeholder='127.0.0.1'
              className='ml-auto h-8 w-56 text-right font-mono text-[12px]'
              onChange={(event) => setHost(event.currentTarget.value)}
            />
          </PreferenceRow>
          <PreferenceRow title={t('settings.api.port')} description={t('settings.api.portHint')}>
            <Input
              type='number'
              min={1}
              max={65535}
              value={port}
              disabled={!enabled}
              aria-invalid={!portValid}
              className='ml-auto h-8 w-32 text-right text-[12px]'
              onChange={(event) => setPort(event.currentTarget.value)}
            />
          </PreferenceRow>
          <PreferenceRow
            title={t('settings.api.key')}
            description={t('settings.api.keyDescription')}
          >
            <ApiKeyField credential={credential} configured={configured} onChange={setCredential} />
          </PreferenceRow>
        </PreferenceSection>

        <p className='mt-2.5 text-[11px] text-muted-foreground'>
          {settings.listening !== null
            ? t('settings.api.listening', {
                url: `http://${settings.host}:${settings.listening}`,
              })
            : t('settings.api.stopped')}
        </p>

        <div className='mt-6 flex items-center gap-3'>
          <Button
            type='button'
            size='sm'
            className='h-9 justify-center gap-1.5 text-[11px]'
            disabled={!portValid || (!changed && !credentialChanged(credential)) || busy}
            onClick={() => void handleApply()}
          >
            {t('settings.api.apply')}
          </Button>
          {!changed && !busy && (
            <span className='text-[11px] text-muted-foreground'>{t('settings.api.saved')}</span>
          )}
        </div>
      </div>
    </PreferencePage>
  )
}

function credentialChanged(credential: CredentialInput): boolean {
  return Boolean(credential.value) || credential.clear
}

function ApiKeyField({
  credential,
  configured,
  onChange,
}: {
  credential: CredentialInput
  configured: boolean
  onChange: (value: CredentialInput) => void
}) {
  const { t } = useTranslation()
  const keyId = useId()
  const [draft, setDraft] = useState(credential.value ?? '')
  useEffect(() => {
    if (credential.value !== null) setDraft(credential.value)
    else if (!credential.configured || credential.clear) setDraft('')
  }, [credential.clear, credential.configured, credential.value])
  return (
    <div className='flex min-w-0 flex-1 items-center gap-2'>
      <Input
        id={keyId}
        type='text'
        autoComplete='off'
        autoCapitalize='none'
        spellCheck={false}
        value={draft}
        placeholder={
          configured ? t('settings.providers.configured') : t('settings.providers.notConfigured')
        }
        className='h-8 min-w-0 flex-1 text-[12px] [-webkit-text-security:disc] [&::placeholder]:[-webkit-text-security:none]'
        onChange={(event) => {
          const value = event.currentTarget.value
          setDraft(value)
          onChange({ ...credential, value: value || null, clear: false })
        }}
      />
      {configured && (
        <Button
          type='button'
          variant='outline'
          size='icon'
          aria-label={t('settings.providers.clearCredential', {
            provider: 'API',
          })}
          onClick={() => {
            setDraft('')
            onChange({ configured: false, value: null, clear: true })
          }}
        >
          <Eraser />
        </Button>
      )}
    </div>
  )
}
