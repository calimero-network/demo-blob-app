import React from 'react';
import { ProtectedRoutesWrapper } from '@calimero-network/calimero-client';
import ChatPage from './pages/ChatPage';

export default function App() {
  return (
    <ProtectedRoutesWrapper 
      permissions={["context:execute", "application", "blob"]} 
      applicationId="Z5LTHsG3ZrTtGZdF6iwXnsr36xgngUtn4VFKAD4ZrUC"
      clientApplicationPath="https://calimero-only-peers-dev.s3.amazonaws.com/uploads/0c04e67a32a51f1867ec07cad4553bab.wasm">
      <ChatPage />
    </ProtectedRoutesWrapper>
  );
}
